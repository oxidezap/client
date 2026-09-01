//! What a picked file becomes on the wire.
//!
//! The mirror of [`super::media`], which reads an arriving message and says
//! what media it carries. This one goes the other way: it takes the bytes
//! somebody chose, works out what the recipient's client needs to draw them
//! before it has downloaded anything, and builds the message that carries it.
//!
//! Two halves, and they are separate on purpose. [`Shape::of`] is pure — bytes
//! in, dimensions and a thumbnail out — so it can be tested without a session,
//! a network or a store; [`message`] is the assembly, and does nothing but
//! move fields. What is *between* them is the upload, which is the session's.
//!
//! # Why the thumbnail is worth the work
//!
//! WhatsApp puts a small JPEG on the message itself, so the other side draws
//! the picture the moment the message arrives and downloads the full file only
//! if somebody opens it. A message sent without one is a grey box until then —
//! and in this tree it is a grey box *here* too, because the store keeps what
//! was sent and hydration reads it back through the same `media_of` an
//! arriving message goes through.

use log::{debug, warn};
use oxidezap_core::{OutgoingMedia, QuotedMessage};
use whatsapp_rust::buffa::MessageField;
use whatsapp_rust::upload::UploadResponse;
use whatsapp_rust::wacore::download::MediaType as DownloadMediaType;
use whatsapp_rust::waproto::whatsapp as wa;

use super::convert::quote_context;

/// A file somebody picked, on its way out.
///
/// Not the protocol's own `SendMedia`: this crate does not depend on the
/// protocol, and the two do not carry the same thing anyway — that one names
/// a staged payload by key, and by the time it reaches here the daemon has
/// taken the bytes out of the cache.
#[derive(Debug, Clone)]
pub struct OutgoingFile {
    /// The whole file. There is no streaming upload a browser can drive, so
    /// this is what the encrypt-and-upload path wants either way.
    pub data: Vec<u8>,
    /// What it is being sent as, which the front end decided.
    pub kind: OutgoingMedia,
    /// The type it was picked as.
    pub mime_type: String,
    /// What it was called where it was picked.
    pub file_name: String,
    /// The line typed beside it, if any.
    pub caption: Option<String>,
}

impl OutgoingFile {
    /// Which key space the CDN files this under.
    fn upload_type(&self) -> DownloadMediaType {
        match self.kind {
            OutgoingMedia::Image => DownloadMediaType::Image,
            OutgoingMedia::Video => DownloadMediaType::Video,
            OutgoingMedia::Document => DownloadMediaType::Document,
        }
    }

    /// Whether the CDN should be asked for the per-64-KiB HMAC table.
    ///
    /// A video only, and for the reason the field exists: it is what lets the
    /// other side start playing before the whole file is down. A document is
    /// opened whole and an image is drawn whole, so asking for one there is
    /// work and bytes for something nothing reads.
    fn wants_streaming_sidecar(&self) -> bool {
        matches!(self.kind, OutgoingMedia::Video)
    }

    /// The upload the CDN needs, described the way the library asks for it.
    pub(super) fn upload_options(&self) -> (DownloadMediaType, whatsapp_rust::UploadOptions) {
        (
            self.upload_type(),
            whatsapp_rust::UploadOptions::new()
                .with_streaming_sidecar(self.wants_streaming_sidecar()),
        )
    }
}

/// The longest edge of the thumbnail that travels on the message.
///
/// Small on purpose: this rides inside the message envelope, which every one
/// of the account's devices receives and stores. It is a placeholder to draw
/// while the real file is fetched, not a preview to look at.
const THUMBNAIL_EDGE: u32 = 96;

/// How hard the thumbnail is compressed.
///
/// Low enough that a photographic thumbnail lands in a couple of kilobytes,
/// high enough that it is recognisably the picture. It is drawn at a fraction
/// of the size it is encoded at, so artefacts at this quality are invisible.
const THUMBNAIL_QUALITY: u8 = 60;

/// Past this, the picture is described but not scaled down.
///
/// Decoding is what costs: four bytes a pixel, allocated inside the process
/// holding the account — in a page, inside a linear memory with a ceiling.
/// Fifty megapixels is past any camera somebody is sending a photo from and
/// is two hundred megabytes decoded, which is the number this is really
/// about. Over it the header still gives the recipient the dimensions and
/// only the thumbnail is skipped, because a picture that cannot be previewed
/// is not a picture that cannot be sent.
const MAX_THUMBNAIL_SOURCE_PIXELS: u64 = 50_000_000;

/// The same ceiling, as the decoder's own limit.
///
/// The dimensions above are what the *header* claims, and a decoder can
/// allocate on its way to disagreeing with one — `image`'s own default is
/// 512 MiB, which is a page's whole heap. This is the belt to that
/// suspenders: a file engineered to be small on disk and enormous in memory
/// fails the decode and is sent without a thumbnail rather than taking the
/// account down with it.
const MAX_THUMBNAIL_DECODE_BYTES: u64 = 4 * MAX_THUMBNAIL_SOURCE_PIXELS;

/// What the bytes say about themselves, beyond what the picker knew.
///
/// Every field is optional and every absence is honest: a container this tree
/// cannot read, a picture too large to decode, a document that has no
/// dimensions to have. Nothing here is invented — a message that says nothing
/// about its size draws as the recipient's client chooses, which is better
/// than one that says the wrong thing.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Shape {
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Whole seconds, for a video.
    pub duration_secs: Option<u32>,
    /// A small JPEG the recipient draws before downloading anything.
    pub thumbnail: Option<Vec<u8>>,
}

impl Shape {
    /// Read what this file can be made to say about itself.
    ///
    /// Never fails: a file whose shape cannot be read is sent with its shape
    /// unstated, which is what every field being an `Option` is for. The
    /// reasons are logged, because "the picture arrived without a preview" is
    /// otherwise indistinguishable from "nobody tried".
    pub(super) fn of(file: &OutgoingFile) -> Self {
        match file.kind {
            OutgoingMedia::Image => still(&file.data),
            OutgoingMedia::Video => moving(&file.data),
            // A document has no dimensions and no poster frame. WhatsApp's own
            // clients render a first page for a PDF; doing that means a PDF
            // renderer in the process holding the account, which is a much
            // larger thing than this.
            OutgoingMedia::Document => Self::default(),
        }
    }
}

/// A picture: its dimensions from the header, its thumbnail from the pixels.
fn still(data: &[u8]) -> Shape {
    let reader = match image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format() {
        Ok(reader) => reader,
        Err(e) => {
            warn!("could not read that image's header: {e}");
            return Shape::default();
        }
    };
    // Kept, because `into_dimensions` consumes the reader and guessing the
    // format again would mean sniffing the same bytes a second time.
    let format = reader.format();
    let (width, height) = match reader.into_dimensions() {
        Ok(dimensions) => dimensions,
        Err(e) => {
            warn!("could not measure that image: {e}");
            return Shape::default();
        }
    };

    let shape = Shape {
        width: Some(width),
        height: Some(height),
        ..Shape::default()
    };
    if u64::from(width) * u64::from(height) > MAX_THUMBNAIL_SOURCE_PIXELS {
        warn!("{width}x{height} is too large to thumbnail; sending it without one");
        return shape;
    }

    let mut reader = image::ImageReader::new(std::io::Cursor::new(data));
    reader.set_format(match format {
        Some(format) => format,
        None => {
            warn!("that image is in a format this build cannot read");
            return shape;
        }
    });
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_THUMBNAIL_DECODE_BYTES);
    reader.limits(limits);
    let decoded = match reader.decode() {
        Ok(decoded) => decoded,
        Err(e) => {
            warn!("could not decode that image for a thumbnail: {e}");
            return shape;
        }
    };

    Shape {
        thumbnail: jpeg_thumbnail(&decoded),
        ..shape
    }
}

/// Scale to [`THUMBNAIL_EDGE`] and encode, or say why not.
fn jpeg_thumbnail(image: &image::DynamicImage) -> Option<Vec<u8>> {
    use image::GenericImageView as _;

    let (width, height) = image.dimensions();
    let (width, height) = fit_within(width, height, THUMBNAIL_EDGE);
    // `thumbnail` rather than `resize`: it is the box filter, which is several
    // times faster than Lanczos and indistinguishable at this size — and this
    // runs on the page's only thread when the page is the one holding the
    // account.
    let small = image.thumbnail(width, height);
    // JPEG has no alpha, and an RGBA source encoded as RGB without being told
    // to drop it is a panic in the encoder rather than a wrong picture.
    let rgb = small.into_rgb8();

    let mut bytes = Vec::new();
    let mut encoder =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, THUMBNAIL_QUALITY);
    match encoder.encode_image(&rgb) {
        Ok(()) => {
            debug!("thumbnail: {}x{} in {} bytes", width, height, bytes.len());
            Some(bytes)
        }
        Err(e) => {
            warn!("could not encode a thumbnail: {e}");
            None
        }
    }
}

/// The largest box within `edge` that keeps the aspect ratio.
///
/// Never zero on either axis: a very wide picture rounds its short side down
/// to nothing, and a zero-sized scale is an error rather than a small image.
fn fit_within(width: u32, height: u32, edge: u32) -> (u32, u32) {
    if width <= edge && height <= edge {
        return (width.max(1), height.max(1));
    }
    let (width, height) = if width >= height {
        (
            edge,
            (u64::from(height) * u64::from(edge) / u64::from(width.max(1))) as u32,
        )
    } else {
        (
            (u64::from(width) * u64::from(edge) / u64::from(height.max(1))) as u32,
            edge,
        )
    };
    (width.max(1), height.max(1))
}

/// A video: what its container will admit to.
///
/// ISO-MP4 only, which is what the picker's own filter asks for and what every
/// phone records. A WebM or a MKV is sent with its shape unstated rather than
/// refused — the recipient's client plays it or does not, and that is a fact
/// about the file rather than about this.
fn moving(data: &[u8]) -> Shape {
    let cursor = std::io::Cursor::new(data);
    let reader = match mp4::Mp4Reader::read_header(cursor, data.len() as u64) {
        Ok(reader) => reader,
        Err(e) => {
            debug!("that video is not an ISO-MP4, so it is sent unmeasured: {e}");
            return Shape::default();
        }
    };
    let Some(track) = reader.tracks().values().find(|track| {
        track
            .track_type()
            .is_ok_and(|kind| kind == mp4::TrackType::Video)
    }) else {
        debug!("that video has no video track to measure");
        return Shape::default();
    };

    Shape {
        width: Some(u32::from(track.width())),
        height: Some(u32::from(track.height())),
        // Rounded up, so a four-and-a-half-second clip is not "4 seconds"
        // where the last frame is: the field is what the other side prints
        // under the poster frame.
        duration_secs: Some(track.duration().as_secs_f64().ceil() as u32),
        // No poster frame. Extracting one means decoding H.264 inside the
        // process holding the account, which is a decoder this crate does not
        // have and a page has only asynchronously.
        thumbnail: None,
    }
}

/// What the CDN answered, in a shape a test can build.
///
/// The library's own `UploadResponse` is `#[non_exhaustive]`, which is right
/// for it — it grows fields — and means nothing outside that crate can
/// construct one. [`message`] is exactly the code worth checking field by
/// field, so it takes this and the one conversion lives here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Uploaded {
    pub url: String,
    pub direct_path: String,
    pub media_key: Vec<u8>,
    pub file_sha256: Vec<u8>,
    pub file_enc_sha256: Vec<u8>,
    pub file_length: u64,
    pub media_key_timestamp: i64,
    pub streaming_sidecar: Option<Vec<u8>>,
}

impl From<UploadResponse> for Uploaded {
    fn from(response: UploadResponse) -> Self {
        Self {
            url: response.url,
            direct_path: response.direct_path,
            media_key: response.media_key.to_vec(),
            file_sha256: response.file_sha256.to_vec(),
            file_enc_sha256: response.file_enc_sha256.to_vec(),
            file_length: response.file_length,
            media_key_timestamp: response.media_key_timestamp,
            streaming_sidecar: response.streaming_sidecar,
        }
    }
}

/// The message that carries this file.
///
/// The three kinds share every CDN field and differ only in what they say
/// about the content, so the fields are filled in once and the kind decides
/// which message they are poured into.
pub(super) fn message(
    file: &OutgoingFile,
    shape: Shape,
    upload: Uploaded,
    quoted: Option<&QuotedMessage>,
) -> wa::Message {
    let context = quoted.map(quote_context);
    let url = Some(upload.url);
    let direct_path = Some(upload.direct_path);
    let media_key = Some(upload.media_key);
    let file_sha256 = Some(upload.file_sha256);
    let file_enc_sha256 = Some(upload.file_enc_sha256);
    let file_length = Some(upload.file_length);
    let media_key_timestamp = Some(upload.media_key_timestamp);
    let mimetype = Some(file.mime_type.clone());
    let caption = file.caption.clone();

    match file.kind {
        OutgoingMedia::Image => wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                url,
                direct_path,
                media_key,
                file_sha256,
                file_enc_sha256,
                file_length,
                media_key_timestamp,
                mimetype,
                caption,
                width: shape.width,
                height: shape.height,
                jpeg_thumbnail: shape.thumbnail,
                context_info: context.into(),
                ..Default::default()
            }),
            ..Default::default()
        },
        OutgoingMedia::Video => wa::Message {
            video_message: MessageField::some(wa::message::VideoMessage {
                url,
                direct_path,
                media_key,
                file_sha256,
                file_enc_sha256,
                file_length,
                media_key_timestamp,
                mimetype,
                caption,
                width: shape.width,
                height: shape.height,
                seconds: shape.duration_secs,
                jpeg_thumbnail: shape.thumbnail,
                // What the CDN handed back for exactly this: without it the
                // other side downloads the whole file before the first frame.
                streaming_sidecar: upload.streaming_sidecar,
                context_info: context.into(),
                ..Default::default()
            }),
            ..Default::default()
        },
        OutgoingMedia::Document => wa::Message {
            document_message: MessageField::some(wa::message::DocumentMessage {
                url,
                direct_path,
                media_key,
                file_sha256,
                file_enc_sha256,
                file_length,
                media_key_timestamp,
                mimetype,
                caption,
                // Both, and they are not the same field: `file_name` is what a
                // save writes and `title` is what the bubble prints. A
                // document with no title draws as its URL in some clients.
                file_name: Some(file.file_name.clone()),
                title: Some(file.file_name.clone()),
                context_info: context.into(),
                ..Default::default()
            }),
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PNG with known dimensions, built rather than committed: a fixture is
    /// a file to keep in step, and this is four lines.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
        });
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("the encoder should write a PNG");
        bytes
    }

    fn file(kind: OutgoingMedia, data: Vec<u8>) -> OutgoingFile {
        OutgoingFile {
            data,
            kind,
            mime_type: "image/png".to_string(),
            file_name: "praia.png".to_string(),
            caption: None,
        }
    }

    /// The dimensions the recipient draws the placeholder at, and a thumbnail
    /// small enough to ride on the message. Without these the other side has a
    /// grey box until somebody opens it.
    #[test]
    fn a_picture_is_measured_and_carries_a_thumbnail() {
        let shape = Shape::of(&file(OutgoingMedia::Image, png(640, 480)));
        assert_eq!((shape.width, shape.height), (Some(640), Some(480)));
        let thumbnail = shape.thumbnail.expect("a picture should get a thumbnail");
        // It has to be a JPEG, because that is the field it goes in.
        assert_eq!(
            image::guess_format(&thumbnail).ok(),
            Some(image::ImageFormat::Jpeg)
        );
        // And small enough to travel: this rides inside the envelope every one
        // of the account's devices receives.
        assert!(
            thumbnail.len() < 16 * 1024,
            "a thumbnail of {} bytes is too big to ride on a message",
            thumbnail.len()
        );
        let (width, height) = image::ImageReader::new(std::io::Cursor::new(&thumbnail))
            .with_guessed_format()
            .expect("the thumbnail should be readable")
            .into_dimensions()
            .expect("the thumbnail should have dimensions");
        assert_eq!(
            (width, height),
            (THUMBNAIL_EDGE, THUMBNAIL_EDGE * 480 / 640)
        );
    }

    /// A file this build cannot read is still sent. Refusing it would be
    /// refusing to send a file for a reason that is about the *preview*.
    #[test]
    fn a_picture_nothing_can_read_is_sent_without_a_shape() {
        let shape = Shape::of(&file(
            OutgoingMedia::Image,
            b"not a picture at all".to_vec(),
        ));
        assert_eq!(shape, Shape::default());
    }

    /// A document has neither dimensions nor a poster frame, and asking the
    /// image decoder about a PDF wastes a decode to learn that.
    #[test]
    fn a_document_is_not_asked_what_it_looks_like() {
        let shape = Shape::of(&file(OutgoingMedia::Document, png(64, 64)));
        assert_eq!(shape, Shape::default());
    }

    /// The one that used to be a panic: JPEG has no alpha channel, and the
    /// encoder refuses an RGBA image rather than dropping it.
    #[test]
    fn a_picture_with_transparency_still_encodes() {
        let mut image = image::RgbaImage::new(32, 32);
        image.put_pixel(0, 0, image::Rgba([255, 0, 0, 0]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("the encoder should write a PNG");
        assert!(
            Shape::of(&file(OutgoingMedia::Image, bytes))
                .thumbnail
                .is_some()
        );
    }

    /// A picture narrower than the thumbnail box keeps both of its edges: a
    /// scale to zero is an error, not a small image.
    #[test]
    fn a_sliver_of_a_picture_keeps_at_least_one_pixel() {
        assert_eq!(fit_within(4000, 3, THUMBNAIL_EDGE), (THUMBNAIL_EDGE, 1));
        assert_eq!(fit_within(3, 4000, THUMBNAIL_EDGE), (1, THUMBNAIL_EDGE));
        // Already smaller than the box: left alone rather than blown up.
        assert_eq!(fit_within(10, 20, THUMBNAIL_EDGE), (10, 20));
    }

    /// The fields the recipient needs to fetch the file, in the message the
    /// kind asks for. A field dropped here is a message that arrives and
    /// cannot be downloaded.
    #[test]
    fn every_kind_carries_what_it_takes_to_fetch_the_file() {
        let upload = Uploaded {
            url: "https://mmg.whatsapp.net/v/t62/upload".to_string(),
            direct_path: "/v/t62/direct".to_string(),
            media_key: vec![1; 32],
            file_sha256: vec![3; 32],
            file_enc_sha256: vec![2; 32],
            file_length: 4242,
            media_key_timestamp: 1_700_000_000,
            streaming_sidecar: Some(vec![9, 9]),
        };

        let image = message(
            &file(OutgoingMedia::Image, Vec::new()),
            Shape {
                width: Some(640),
                height: Some(480),
                duration_secs: None,
                thumbnail: Some(vec![0xff, 0xd8]),
            },
            upload.clone(),
            None,
        );
        let image = image.image_message.as_option().expect("an image message");
        assert_eq!(image.direct_path.as_deref(), Some("/v/t62/direct"));
        assert_eq!(image.media_key.as_deref(), Some(&[1u8; 32][..]));
        assert_eq!(image.file_length, Some(4242));
        assert_eq!(image.media_key_timestamp, Some(1_700_000_000));
        assert_eq!(image.width, Some(640));
        assert!(image.jpeg_thumbnail.is_some());

        let video = message(
            &OutgoingFile {
                kind: OutgoingMedia::Video,
                mime_type: "video/mp4".to_string(),
                ..file(OutgoingMedia::Video, Vec::new())
            },
            Shape {
                width: Some(640),
                height: Some(480),
                duration_secs: Some(12),
                thumbnail: None,
            },
            upload.clone(),
            None,
        );
        let video = video.video_message.as_option().expect("a video message");
        assert_eq!(video.seconds, Some(12));
        // The table that lets the other side start playing before the whole
        // file is down. Asked for at the upload, so it has to be carried here.
        assert_eq!(video.streaming_sidecar.as_deref(), Some(&[9u8, 9][..]));

        let document = message(
            &OutgoingFile {
                kind: OutgoingMedia::Document,
                mime_type: "application/pdf".to_string(),
                file_name: "nota fiscal.pdf".to_string(),
                caption: Some("segue".to_string()),
                ..file(OutgoingMedia::Document, Vec::new())
            },
            Shape::default(),
            upload,
            None,
        );
        let document = document
            .document_message
            .as_option()
            .expect("a document message");
        assert_eq!(document.file_name.as_deref(), Some("nota fiscal.pdf"));
        assert_eq!(document.title.as_deref(), Some("nota fiscal.pdf"));
        assert_eq!(document.caption.as_deref(), Some("segue"));
    }

    /// A reply carries its quote whatever it is a reply *with*. A draft open
    /// when the paperclip was pressed belongs to that send, and dropping it
    /// here is the bug the voice-note path already had once.
    #[test]
    fn an_attachment_can_be_a_reply() {
        let quoted = QuotedMessage {
            message_id: "3EB0A".to_string(),
            sender: "559900000001@s.whatsapp.net".to_string(),
            sender_name: "quem quer que seja".to_string(),
            preview: "a linha citada".to_string(),
            kind: None,
        };
        let sent = message(
            &file(OutgoingMedia::Image, Vec::new()),
            Shape::default(),
            Uploaded {
                url: String::new(),
                direct_path: String::new(),
                media_key: Vec::new(),
                file_sha256: Vec::new(),
                file_enc_sha256: Vec::new(),
                file_length: 0,
                media_key_timestamp: 0,
                streaming_sidecar: None,
            },
            Some(&quoted),
        );
        let context = sent
            .image_message
            .as_option()
            .expect("an image message")
            .context_info
            .as_option()
            .expect("a reply should carry its quote");
        assert_eq!(context.stanza_id.as_deref(), Some("3EB0A"));
    }
}
