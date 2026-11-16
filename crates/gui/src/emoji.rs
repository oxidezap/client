//! The application supplies the emoji design; the UI font supplies ordinary text.

use std::ops::Range;

use gpui_component::input::{InlineToken, InputContent};
use unicode_segmentation::UnicodeSegmentation as _;

pub(crate) const FAMILY: &str = "OxideZap Emoji";
/// Match the bundled artwork, rather than offering newer, unsupported glyphs.
pub(crate) const VERSION: emojis::EmojiVersion = emojis::EmojiVersion::new(17, 0);

pub(crate) fn ranges(text: &str) -> Vec<Range<usize>> {
    if text.is_ascii() {
        return Vec::new();
    }
    text.grapheme_indices(true)
        .filter(|(_, cluster)| !cluster.contains('\u{fe0e}') && emojis::get(cluster).is_some())
        .map(|(start, cluster)| start..start + cluster.len())
        .collect()
}

/// Draft restoration keeps the same Unicode text, with presentation attached.
pub(crate) fn content(text: &str) -> InputContent {
    ranges(text)
        .into_iter()
        .fold(InputContent::new(text.to_owned()), |content, range| {
            let emoji = &text[range.clone()];
            content
                .with_token(range, InlineToken::new("emoji", emoji.to_owned()))
                .expect("emoji spans are disjoint complete graphemes")
        })
}

/// Extract the original PNG for a shaped glyph from the bundled sbix table.
/// Bounds checks keep a missing or truncated web asset from breaking a frame.
fn glyph_png(face: &[u8], glyph: usize) -> Option<&[u8]> {
    fn word(bytes: &[u8], offset: usize) -> Option<usize> {
        Some(u32::from_be_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?) as usize)
    }
    let tables = u16::from_be_bytes(face.get(4..6)?.try_into().ok()?) as usize;
    let entry = (0..tables)
        .map(|ix| 12 + ix * 16)
        .find(|&offset| face.get(offset..offset + 4) == Some(b"sbix"))?;
    let start = word(face, entry + 8)?;
    let table = face.get(start..start.checked_add(word(face, entry + 12)?)?)?;
    let strike = table.get(word(table, 8)?..)?;
    let start = word(strike, 4 + glyph.checked_mul(4)?)?;
    let end = word(strike, 8 + glyph.checked_mul(4)?)?;
    let record = strike.get(start..end)?;
    (record.get(4..8)? == b"png ").then_some(record.get(8..)?)
}

#[derive(Default)]
struct Artwork(indexmap::IndexMap<String, crate::retained_image::RetainedImage>);
impl gpui::Global for Artwork {}

/// Controls use images, with a bounded cache and the normal image retirement
/// leases. Shaping resolves joiners and modifiers before selecting the PNG.
pub(crate) fn picture(
    text: &str,
    size: impl Into<gpui::Length> + Clone,
    window: &mut gpui::Window,
    cx: &mut gpui::App,
) -> gpui::Div {
    use gpui::prelude::*;
    let cached = cx.default_global::<Artwork>().0.get(text).cloned();
    let artwork = cached.or_else(|| {
        let face = crate::platform::fonts::emoji_bytes()?;
        let runs = [gpui::TextRun {
            len: text.len(),
            font: gpui::font(FAMILY),
            color: gpui::Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }];
        let line = window
            .text_system()
            .layout_line(text, window.rem_size(), &runs, None);
        let glyph = line
            .runs
            .iter()
            .flat_map(|run| &run.glyphs)
            .find(|glyph| glyph.is_emoji && glyph.id.0 != 0)?;
        let png = glyph_png(face, glyph.id.0 as usize)?;
        let image = crate::retained_image::RetainedImage::new(gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            png.to_vec(),
        ));
        let cache = &mut cx.default_global::<Artwork>().0;
        if cache.len() == 128 {
            cache.shift_remove_index(0);
        }
        cache.insert(text.to_owned(), image.clone());
        Some(image)
    });
    gpui::div()
        .size(size)
        .flex_shrink_0()
        .children(artwork.map(|image| {
            gpui::img(image.source(false))
                .size_full()
                .object_fit(gpui::ObjectFit::Contain)
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_offered_emoji_has_a_single_bundled_png() {
        use gpui::PlatformTextSystem as _;
        let face = crate::platform::fonts::emoji_bytes().unwrap();
        let backend = std::sync::Arc::new(gpui_wgpu::CosmicTextSystem::new_without_system_fonts(
            FAMILY,
        ));
        backend
            .add_fonts(vec![std::borrow::Cow::Borrowed(face)])
            .unwrap();
        let direct = backend.font_id(&gpui::font(FAMILY)).unwrap();
        let system =
            gpui::WindowTextSystem::new(std::sync::Arc::new(gpui::TextSystem::new(backend)));
        let mut checked = 0;
        for base in emojis::iter().filter(|e| e.emoji_version() <= VERSION) {
            let variants: Vec<_> = base
                .skin_tones()
                .map(|tones| tones.collect())
                .unwrap_or_else(|| vec![base]);
            for emoji in variants {
                let text = emoji.as_str();
                let runs = [gpui::TextRun {
                    len: text.len(),
                    font: gpui::font(FAMILY),
                    color: gpui::Hsla::default(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }];
                let line = system.layout_line(text, gpui::px(32.), &runs, None);
                assert!(
                    line.runs.iter().all(|run| run.font_id == direct),
                    "{text} used fallback"
                );
                let glyphs: Vec<_> = line.runs.iter().flat_map(|run| &run.glyphs).collect();
                assert_eq!(glyphs.len(), 1, "{text} did not form one emoji");
                let png = glyph_png(face, glyphs[0].id.0 as usize).expect(text);
                assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "{text} has no PNG");
                checked += 1;
            }
        }
        assert!(checked > 3500);
        assert!(glyph_png(&[], 0).is_none());
        assert!(glyph_png(face, 100_000).is_none());
    }

    #[test]
    fn emoji_ranges_keep_entire_clusters_and_respect_text_presentation() {
        let text = "hi 👩🏽‍💻 🇧🇷 1️⃣ ❤️ ☺\u{fe0e}";
        let found: Vec<_> = ranges(text).into_iter().map(|r| &text[r]).collect();
        assert_eq!(found, ["👩🏽‍💻", "🇧🇷", "1️⃣", "❤️"]);
        let draft = content(text);
        assert_eq!(draft.text().as_ref(), text);
        assert_eq!(draft.tokens().len(), 4);
        assert!(ranges("ordinary 123 text").is_empty());
    }
}
