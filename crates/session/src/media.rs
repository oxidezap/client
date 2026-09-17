//! Image formats, named rather than sniffed.
//!
//! WhatsApp's own clients accept exactly four picture formats — the strict
//! image guard in the web bundle allows `image/jpeg`, `image/png`,
//! `image/webp` and `image/gif`, and everything else travels as a document.
//! This module is that answer as code: [`named_image_format`] reads only
//! magic bytes, so naming a format links no decoder, and only the four in
//! [`allowed_image_format`] ever reach one, whatever feature set the binary
//! that links this crate was built with (a workspace build unifies `image`
//! features across binaries, so guessing would drag EXR, TIFF and the rest
//! into a process that can never legitimately receive them).

use image::ImageFormat;
use image::codecs::{gif::GifDecoder, jpeg::JpegDecoder, png::PngDecoder, webp::WebPDecoder};

/// What the bytes claim to be, from the magic alone.
///
/// Pure byte matching: no decoder is instantiated, so every variant named
/// here costs one enum discriminant no matter what the binary links.
pub fn named_image_format(data: &[u8]) -> Option<ImageFormat> {
    image::guess_format(data).ok()
}

/// The formats this build decodes: the four WhatsApp's own clients accept.
///
/// A picture in any other format is still sent — with its bytes untouched
/// and its type corrected — but never decoded here.
pub fn allowed_image_format(data: &[u8]) -> Option<ImageFormat> {
    named_image_format(data).filter(|format| {
        matches!(
            format,
            ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::Gif | ImageFormat::WebP
        )
    })
}

/// The dimensions named bytes declare, without decoding their pixels.
///
/// Each format goes to its own decoder rather than through
/// `ImageReader`, whose dispatch constructs every compiled decoder and
/// would link the ones nothing here may receive. The limits are enforced
/// exactly as that dispatch would enforce them.
pub fn dimensions(
    data: &[u8],
    format: ImageFormat,
    limits: image::Limits,
) -> image::ImageResult<(u32, u32)> {
    use image::ImageDecoder as _;

    let cursor = std::io::Cursor::new(data);
    match format {
        ImageFormat::Jpeg => {
            let mut decoder = JpegDecoder::new(cursor)?;
            decoder.set_limits(limits)?;
            Ok(decoder.dimensions())
        }
        // with_limits, not new plus set_limits: the limits also cap the
        // png crate's internal buffers while the header parses, and
        // set_limits alone cannot constrain those after the fact — the
        // constructor is what the dispatched path used too.
        ImageFormat::Png => {
            let decoder = PngDecoder::with_limits(cursor, limits)?;
            Ok(decoder.dimensions())
        }
        ImageFormat::Gif => {
            let mut decoder = GifDecoder::new(cursor)?;
            decoder.set_limits(limits)?;
            Ok(decoder.dimensions())
        }
        ImageFormat::WebP => {
            let mut decoder = WebPDecoder::new(cursor)?;
            decoder.set_limits(limits)?;
            Ok(decoder.dimensions())
        }
        _ => Err(unsupported(format)),
    }
}

/// The pixels named bytes carry, within the limits given.
///
/// Same direct dispatch as [`dimensions`]: the pre-flight reservation and
/// the limits are enforced exactly as `ImageReader::decode` would enforce
/// them.
pub fn decode(
    data: &[u8],
    format: ImageFormat,
    limits: image::Limits,
) -> image::ImageResult<image::DynamicImage> {
    let cursor = std::io::Cursor::new(data);
    match format {
        ImageFormat::Jpeg => decode_with(JpegDecoder::new(cursor)?, limits),
        ImageFormat::Png => decode_with(PngDecoder::with_limits(cursor, limits.clone())?, limits),
        ImageFormat::Gif => decode_with(GifDecoder::new(cursor)?, limits),
        ImageFormat::WebP => decode_with(WebPDecoder::new(cursor)?, limits),
        _ => Err(unsupported(format)),
    }
}

/// Decode through one decoder the way `ImageReader::decode` would: refuse
/// the allocation before it happens, then let the decoder enforce the rest.
fn decode_with(
    mut decoder: impl image::ImageDecoder,
    mut limits: image::Limits,
) -> image::ImageResult<image::DynamicImage> {
    limits.reserve(decoder.total_bytes())?;
    decoder.set_limits(limits)?;
    image::DynamicImage::from_decoder(decoder)
}

/// The error for a format that names but never decodes here.
///
/// Unreachable through [`allowed_image_format`], which filters to the four
/// above — but a total function rather than a panic, because a picture is
/// never a reason to stop the process.
fn unsupported(format: ImageFormat) -> image::ImageError {
    use image::error::{UnsupportedError, UnsupportedErrorKind};

    image::ImageError::Unsupported(UnsupportedError::from_format_and_kind(
        format.into(),
        UnsupportedErrorKind::Format(image::error::ImageFormatHint::Exact(format)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Magic alone is enough to name a format: no decoder runs here.
    #[test]
    fn naming_reads_only_magic_bytes() {
        let cases: &[(&[u8], ImageFormat)] = &[
            (b"\xFF\xD8\xFF\x00", ImageFormat::Jpeg),
            (b"\x89PNG\r\n\x1a\n", ImageFormat::Png),
            (b"GIF89a", ImageFormat::Gif),
            (b"RIFF\x00\x00\x00\x00WEBP", ImageFormat::WebP),
            // Named but never decoded here: the mime is still worth saying.
            (b"BM\x00\x00\x00\x00", ImageFormat::Bmp),
            (b"II*\x00", ImageFormat::Tiff),
            (b"MM\x00*", ImageFormat::Tiff),
            (b"v/1\x01", ImageFormat::OpenExr),
        ];
        for (bytes, expected) in cases {
            assert_eq!(
                named_image_format(bytes),
                Some(*expected),
                "should name {expected:?} from its magic"
            );
        }
        assert_eq!(named_image_format(b"not a picture at all"), None);
        assert_eq!(named_image_format(b""), None);
    }

    /// Only WhatsApp's four picture formats decode here.
    #[test]
    fn decoding_is_the_allowlist_and_nothing_else() {
        for bytes in [
            b"\xFF\xD8\xFF\x00".as_slice(),
            b"\x89PNG\r\n\x1a\n",
            b"GIF89a",
            b"RIFF\x00\x00\x00\x00WEBP",
        ] {
            assert!(
                allowed_image_format(bytes).is_some(),
                "should decode {bytes:?}"
            );
        }
        for bytes in [
            b"BM\x00\x00\x00\x00".as_slice(),
            b"II*\x00",
            b"v/1\x01",
            b"not a picture at all",
        ] {
            assert_eq!(
                allowed_image_format(bytes),
                None,
                "should not decode {bytes:?}"
            );
        }
    }
}
