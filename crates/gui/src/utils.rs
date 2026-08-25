//! Shared utility functions for the WhatsApp UI

use chrono::{DateTime, Local, Utc};
use gpui::ImageFormat;

/// Convert a MIME type string to a GPUI ImageFormat
pub fn mime_to_image_format(mime: &str) -> ImageFormat {
    match mime {
        // image/jpg is non-standard but some senders emit it
        "image/jpeg" | "image/jpg" => ImageFormat::Jpeg,
        "image/png" => ImageFormat::Png,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::Webp,
        "image/bmp" => ImageFormat::Bmp,
        _ => {
            log::warn!("unrecognized image MIME type {mime}, falling back to PNG");
            ImageFormat::Png
        }
    }
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
    let today = whatsapp_rust::wacore::time::now_utc()
        .with_timezone(&Local)
        .date_naive();
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
    let today = whatsapp_rust::wacore::time::now_utc()
        .with_timezone(&Local)
        .date_naive();

    match (today - local.date_naive()).num_days() {
        0 => local.format("Today at %H:%M").to_string(),
        1 => local.format("Yesterday at %H:%M").to_string(),
        _ => local.format("%d/%m/%Y at %H:%M").to_string(),
    }
}

/// The heading over a group of messages sent on the same day.
pub fn format_date_divider(timestamp: &DateTime<Utc>) -> String {
    let local: DateTime<Local> = timestamp.with_timezone(&Local);
    let today = whatsapp_rust::wacore::time::now_utc()
        .with_timezone(&Local)
        .date_naive();

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
    use super::scale_media_dimensions;

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
