//! Shared utility functions for the WhatsApp UI

use chrono::{DateTime, Local, Utc};
use gpui::ImageFormat;

/// How GPUI should decode these bytes, when they are a still picture at all.
///
/// `None` is the answer for everything that is not one, and it is a real
/// answer rather than a failure: a row's `data` is whatever can be *shown*
/// for it, and for a video that is a poster thumbnail right up until the file
/// itself is fetched — after which [`MediaContent::adopt_full_bytes`] puts
/// the MP4 in `data` and `video/mp4` beside it. Every surface that draws a
/// still asks here first, so the one that has a frame to draw instead falls
/// through to it.
///
/// [`MediaContent::adopt_full_bytes`]: oxidezap_core::MediaContent::adopt_full_bytes
pub fn mime_to_image_format(mime: &str) -> Option<ImageFormat> {
    // The type, without its parameters and without regard to case: both are
    // the sender's to choose (RFC 2045 §5.1), and `Image/JPEG` or
    // `image/jpeg; charset=binary` are the same photo as `image/jpeg`.
    // Compared rather than lowercased into a `String`, because this is asked
    // per row per frame.
    let essence = mime.split(';').next().unwrap_or(mime).trim();
    for (name, format) in [
        ("image/jpeg", ImageFormat::Jpeg),
        // image/jpg is non-standard but some senders emit it
        ("image/jpg", ImageFormat::Jpeg),
        ("image/png", ImageFormat::Png),
        ("image/gif", ImageFormat::Gif),
        ("image/webp", ImageFormat::Webp),
        ("image/bmp", ImageFormat::Bmp),
    ] {
        if essence.eq_ignore_ascii_case(name) {
            return Some(format);
        }
    }

    // A picture in a subtype we do not name. PNG is a guess, and one worth
    // making: the bytes claim to be an image, and GPUI sniffing them itself
    // is a better outcome than a row that draws nothing.
    if essence
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
    {
        log::warn!("unrecognized image MIME type {mime}, falling back to PNG");
        return Some(ImageFormat::Png);
    }

    None
}

/// Scale media dimensions to fit within `max_size` without upscaling, with a
/// ~50px floor on the short side. Both fit and floor are single uniform
/// factors so aspect ratio is always preserved; the floor yields to the
/// `max_size` cap, so a pathological ratio (e.g. 200x20) keeps its shape and
/// accepts a sub-50px short side instead of stretching or overflowing.
pub fn scale_media_dimensions(width: u32, height: u32, max_size: f32) -> (f32, f32) {
    let w = width.max(1) as f32;
    let h = height.max(1) as f32;
    let fit = (max_size / w).min(max_size / h).min(1.0);
    let floor = 50.0 / (w.min(h) * fit);
    let cap = (max_size / (w.max(h) * fit)).max(1.0);
    let scale = fit * floor.clamp(1.0, cap);
    (w * scale, h * scale)
}

/// How a timestamp reads in a conversation list.
///
/// The list answers "when", not "exactly when": today only needs a clock,
/// this week only needs a weekday, and anything older needs a date. Spelling
/// out a full date on every row would make the column unscannable.
pub fn format_list_time(timestamp: &DateTime<Utc>) -> String {
    let local: DateTime<Local> = timestamp.with_timezone(&Local);
    let today = wacore::time::now_utc().with_timezone(&Local).date_naive();
    let date = local.date_naive();

    match (today - date).num_days() {
        0 => local.format("%H:%M").to_string(),
        1 => "Yesterday".to_string(),
        // Inside the last week a weekday is both shorter and easier to place
        // than a date.
        2..=6 => local.format("%a").to_string(),
        _ => local.format("%d/%m/%Y").to_string(),
    }
}

/// When a status update was posted.
///
/// Always with a clock time, unlike a chat row: a status lives 24 hours, so
/// every one of them is either today or yesterday and "Yesterday" on its own
/// narrows it to nothing. The one that matters is how long is left, and that
/// is what the hour tells you.
pub fn format_status_time(timestamp: &DateTime<Utc>) -> String {
    let local: DateTime<Local> = timestamp.with_timezone(&Local);
    let today = wacore::time::now_utc().with_timezone(&Local).date_naive();

    match (today - local.date_naive()).num_days() {
        0 => local.format("Today at %H:%M").to_string(),
        1 => local.format("Yesterday at %H:%M").to_string(),
        _ => local.format("%d/%m/%Y at %H:%M").to_string(),
    }
}

/// The heading over a group of messages sent on the same day.
pub fn format_date_divider(timestamp: &DateTime<Utc>) -> String {
    let local: DateTime<Local> = timestamp.with_timezone(&Local);
    let today = wacore::time::now_utc().with_timezone(&Local).date_naive();

    match (today - local.date_naive()).num_days() {
        0 => "TODAY".to_string(),
        1 => "YESTERDAY".to_string(),
        2..=6 => local.format("%A").to_string().to_uppercase(),
        _ => local.format("%d %b %Y").to_string().to_uppercase(),
    }
}

/// Whether two messages fall on different local days, and so need a divider
/// between them.
pub fn crosses_day(previous: &DateTime<Utc>, current: &DateTime<Utc>) -> bool {
    previous.with_timezone(&Local).date_naive() != current.with_timezone(&Local).date_naive()
}

/// Format a UTC timestamp as local time (HH:MM format).
///
/// Converts from UTC to the system's local timezone before formatting.
/// This ensures timestamps are displayed correctly regardless of where
/// the user is located.
pub fn format_time_local(timestamp: &DateTime<Utc>) -> String {
    let local: DateTime<Local> = timestamp.with_timezone(&Local);
    local.format("%H:%M").to_string()
}

#[cfg(test)]
mod tests {
    /// A push name is whatever the peer typed and the wire carries it whole:
    /// three hundred characters turned the sidebar row into nothing but the
    /// name, with the preview squeezed to zero and the message gone from the
    /// list, and pushed the call card's pill off the left of the window.
    #[test]
    fn a_name_from_the_peer_cannot_be_any_length() {
        let flood = "a".repeat(300);
        let capped = super::capped_name(&flood);
        assert_eq!(capped.chars().count(), super::MAX_NAME_CHARS + 1);
        assert!(capped.ends_with('\u{2026}'));

        // An ordinary name is handed back exactly as it was.
        assert_eq!(super::capped_name("Ana Souza"), "Ana Souza");
    }

    /// Counted in characters, never in bytes: most of the world's names are
    /// not ASCII, and cutting a UTF-8 sequence in half panics.
    #[test]
    fn a_name_is_cut_between_characters() {
        let accented = "á".repeat(super::MAX_NAME_CHARS + 10);
        let capped = super::capped_name(&accented);
        assert_eq!(capped.chars().count(), super::MAX_NAME_CHARS + 1);
        // Emoji are several bytes and one character each.
        let emoji = "🇧🇷".repeat(super::MAX_NAME_CHARS);
        assert!(super::capped_name(&emoji).chars().count() <= super::MAX_NAME_CHARS + 1);
    }

    use super::{mime_to_image_format, scale_media_dimensions};
    use gpui::ImageFormat;

    #[test]
    fn names_the_formats_senders_actually_use() {
        assert_eq!(mime_to_image_format("image/jpeg"), Some(ImageFormat::Jpeg));
        // Non-standard, and emitted anyway.
        assert_eq!(mime_to_image_format("image/jpg"), Some(ImageFormat::Jpeg));
        assert_eq!(mime_to_image_format("image/webp"), Some(ImageFormat::Webp));
    }

    #[test]
    fn guesses_only_within_images() {
        assert_eq!(mime_to_image_format("image/avif"), Some(ImageFormat::Png));
    }

    /// Case and parameters are the sender's to choose, and neither changes
    /// what the bytes are.
    #[test]
    fn the_type_is_read_as_the_sender_may_spell_it() {
        assert_eq!(mime_to_image_format("Image/JPEG"), Some(ImageFormat::Jpeg));
        assert_eq!(
            mime_to_image_format("image/webp; charset=binary"),
            Some(ImageFormat::Webp)
        );
        assert_eq!(mime_to_image_format("IMAGE/AVIF"), Some(ImageFormat::Png));
        assert_eq!(mime_to_image_format("Video/MP4"), None);
    }

    #[test]
    fn a_video_is_not_a_still() {
        // What a status posted as a photo with music arrives as: an MP4 with
        // two keyframes. Decoding it as a picture is a guess that can only
        // draw nothing, and the surfaces that ask have a frame to draw.
        assert_eq!(mime_to_image_format("video/mp4"), None);
        assert_eq!(mime_to_image_format("audio/ogg; codecs=opus"), None);
        assert_eq!(mime_to_image_format("application/pdf"), None);
    }

    fn assert_close(actual: (f32, f32), expected: (f32, f32)) {
        assert!(
            (actual.0 - expected.0).abs() < 0.01 && (actual.1 - expected.1).abs() < 0.01,
            "{actual:?} != {expected:?}"
        );
    }

    #[test]
    fn shrinks_large_media_uniformly() {
        assert_close(scale_media_dimensions(4000, 3000, 300.0), (300.0, 225.0));
    }

    #[test]
    fn floors_tiny_media_uniformly() {
        assert_close(scale_media_dimensions(10, 10, 300.0), (50.0, 50.0));
    }

    #[test]
    fn extreme_ratio_keeps_shape_and_respects_cap() {
        // 10:1 stays 10:1; the floor grow stops at max_size instead of
        // stretching only the short side (the old per-axis behavior).
        assert_close(scale_media_dimensions(200, 20, 300.0), (300.0, 30.0));
    }
}

/// Longest a person's name may be where one is drawn.
///
/// A push name is whatever the peer typed and the wire carries it whole. It
/// reaches the sidebar's preview line and the call card's pill, and both are
/// laid out around it: a three-hundred-character name turns the sidebar row
/// into nothing but the name, with the preview squeezed to zero, and pushes
/// the call card off the left edge of the window with its drag handle. Sixty
/// four is far past any name anybody chooses and short enough that the
/// layout still owns the row.
const MAX_NAME_CHARS: usize = 64;

/// A name at a length a layout can hold.
///
/// Counted in `chars`, never in bytes: a name is somebody's, most of the
/// world's are not ASCII, and cutting a UTF-8 sequence in half panics.
/// Truncated here rather than only in the component, because the string is
/// also measured, compared and put in tooltips.
#[must_use]
pub fn capped_name(name: &str) -> String {
    let mut chars = name.chars();
    let head: String = chars.by_ref().take(MAX_NAME_CHARS).collect();
    if chars.next().is_none() {
        head
    } else {
        format!("{head}\u{2026}")
    }
}
