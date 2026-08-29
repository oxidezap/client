//! Message text, drawn the way a phone draws it.
//!
//! WhatsApp's markup (`*bold*`, `_italic_`, `~strike~`, `` `code` ``) is
//! formatting on every other client, so a message that shows the markers is
//! showing its working. The rules live in [`oxidezap_core::parse_rich_text`],
//! which is where they belong — they are a property of the text and not of a
//! bubble — and this is the one place that turns the parsed spans into
//! something GPUI paints.
//!
//! Ordinary text takes the cheap path: no markers means no spans, and a plain
//! string goes straight into a `div` with no highlight vector built and no
//! second string allocated.

use gpui::{
    App, FontStyle, FontWeight, HighlightStyle, IntoElement, SharedString, StrikethroughStyle,
    StyledText, px,
};
use gpui_component::ActiveTheme as _;

use oxidezap_core::{Emphasis, parse_rich_text};

/// Message text with its markup applied.
///
/// Returns an element either way: the caller styles size and colour on the
/// parent, and both paths inherit it.
pub fn render_rich_text(source: &str, cx: &App) -> gpui::AnyElement {
    let rich = parse_rich_text(source);
    if rich.is_plain() {
        // Nothing to say about any range, so say nothing: `StyledText` with an
        // empty highlight list still walks and allocates runs.
        return SharedString::from(rich.text).into_any_element();
    }

    let runs = rich.runs();
    let text: SharedString = rich.text.into();
    let mono = cx.theme().mono_font_family.clone();
    // Two passes over the same partition, because GPUI takes the font family
    // apart from the rest of the style: highlights resolve against the
    // inherited text style, family overrides are applied at layout.
    let code: Vec<_> = runs
        .iter()
        .filter(|(_, emphasis)| emphasis.code)
        .map(|(range, _)| (range.clone(), mono.clone()))
        .collect();
    let highlights: Vec<_> = runs
        .into_iter()
        .map(|(range, emphasis)| (range, style_for(emphasis)))
        .collect();

    StyledText::new(text)
        .with_highlights(highlights)
        .with_font_family_overrides(code)
        .into_any_element()
}

/// One run's appearance.
///
/// Weight, slant and the strikethrough come from the font; a code run is only
/// a family swap, applied separately. Deliberately no colour of its own: this
/// text is painted on three different grounds (the sent bubble's brand hue,
/// the received bubble, a quote), and a tint chosen here would be checked
/// against none of them.
fn style_for(emphasis: Emphasis) -> HighlightStyle {
    HighlightStyle {
        font_weight: emphasis.bold.then_some(FontWeight::BOLD),
        font_style: emphasis.italic.then_some(FontStyle::Italic),
        strikethrough: emphasis.strikethrough.then(|| StrikethroughStyle {
            thickness: px(1.),
            color: None,
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    /// A stopwatch rather than an assertion: what a conversation pays per
    /// frame to re-derive text nothing changed.
    ///
    /// `render_rich_text` parses on every render of every visible bubble, and
    /// the header parses its JID once. `cargo test -p oxidezap-gui --
    /// --ignored --nocapture per_frame_text_costs`
    #[test]
    #[ignore = "a measurement, not an assertion"]
    fn per_frame_text_costs() {
        const BUBBLES: usize = 40;
        const FRAMES: usize = 100;

        let plain = "thanks, that works for me";
        let marked = "*thanks*, that _works_ for me, see `run.sh`";

        for (what, source) in [("plain", plain), ("marked", marked)] {
            let started = wacore::time::Instant::now();
            let mut runs = 0;
            for _ in 0..FRAMES {
                for _ in 0..BUBBLES {
                    runs += oxidezap_core::parse_rich_text(source).runs().len();
                }
            }
            let elapsed = started.elapsed();
            println!(
                "{what}: {BUBBLES} bubbles x {FRAMES} frames: {elapsed:?} ({:?} per frame, \
                 {runs} runs)",
                elapsed / FRAMES as u32
            );
        }

        let started = wacore::time::Instant::now();
        let mut callable = 0;
        for _ in 0..FRAMES {
            callable += usize::from(
                "5521999999999@s.whatsapp.net"
                    .parse::<wacore_binary::jid::Jid>()
                    .is_ok(),
            );
        }
        println!(
            "header JID: {FRAMES} frames: {:?} ({:?} per frame, {callable} parsed)",
            started.elapsed(),
            started.elapsed() / FRAMES as u32
        );
    }
}
