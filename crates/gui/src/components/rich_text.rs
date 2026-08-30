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

use std::ops::Range;
use std::sync::Arc;

use gpui::{
    App, FontStyle, FontWeight, HighlightStyle, IntoElement, SharedString, StrikethroughStyle,
    StyledText,
};
use gpui_component::ActiveTheme as _;

use crate::theme::ActiveProductTheme as _;

use oxidezap_core::{Emphasis, parse_rich_text};

/// One message's text, parsed once.
///
/// The markup is a property of the text and the text does not change, so
/// deriving it belongs where the rows are built rather than where they are
/// drawn — the same argument [`BubbleIds`](crate::app::BubbleIds) is built
/// on, and a stronger one: a bubble's ids are a `format!` and this is a scan
/// of a peer's message plus the partition it resolves to, run for every
/// visible bubble of every frame.
///
/// What is *not* in here is the appearance. A `HighlightStyle` resolves
/// against the theme and the metrics, both of which can change under a
/// timeline nothing else invalidates — so this holds what the parse answered
/// and [`render_rich_text`] turns that into runs against the theme of the
/// frame that asks.
#[derive(Clone, Default)]
pub struct BubbleText {
    /// What the reader sees: the source with the markup characters removed.
    text: SharedString,
    /// The partition, empty for ordinary text — which is the common case, and
    /// the case that then costs one refcount per frame and nothing else.
    ///
    /// Shared rather than owned because a `BubbleProps` is built per visible
    /// row per frame and this travels in it.
    runs: Arc<[(Range<usize>, Emphasis)]>,
}

impl BubbleText {
    /// Parse `source` once, for the timeline that will draw it many times.
    pub fn of(source: &str) -> Self {
        let rich = parse_rich_text(source);
        if rich.is_plain() {
            return Self {
                text: rich.text.into(),
                runs: Arc::from([]),
            };
        }
        let runs = rich.runs();
        Self {
            text: rich.text.into(),
            runs: runs.into(),
        }
    }

    /// Whether there is anything to draw. The bubble asks before it builds a
    /// text box at all, because a media message routinely has no caption.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// Message text with its markup applied.
///
/// Returns an element either way: the caller styles size and colour on the
/// parent, and both paths inherit it.
pub fn render_rich_text(parsed: &BubbleText, cx: &App) -> gpui::AnyElement {
    if parsed.runs.is_empty() {
        // Nothing to say about any range, so say nothing: `StyledText` with an
        // empty highlight list still walks and allocates runs.
        return parsed.text.clone().into_any_element();
    }

    let runs = &parsed.runs;
    let text: SharedString = parsed.text.clone();
    let mono = cx.theme().mono_font_family.clone();
    let metrics = cx.product().metrics;
    // Two passes over the same partition, because GPUI takes the font family
    // apart from the rest of the style: highlights resolve against the
    // inherited text style, family overrides are applied at layout.
    let code: Vec<_> = runs
        .iter()
        .filter(|(_, emphasis)| emphasis.code)
        .map(|(range, _)| (range.clone(), mono.clone()))
        .collect();
    let highlights: Vec<_> = runs
        .iter()
        .map(|(range, emphasis)| (range.clone(), style_for(*emphasis, metrics)))
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
fn style_for(emphasis: Emphasis, metrics: crate::theme::Metrics) -> HighlightStyle {
    HighlightStyle {
        font_weight: emphasis.bold.then_some(FontWeight::BOLD),
        font_style: emphasis.italic.then_some(FontStyle::Italic),
        strikethrough: emphasis.strikethrough.then(|| StrikethroughStyle {
            thickness: metrics.hairline(),
            color: None,
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    /// A stopwatch rather than an assertion: what a conversation pays to
    /// re-derive text nothing changed, and what it pays now that it does not.
    ///
    /// The first number is what a frame cost while `render_rich_text` parsed
    /// its source — every visible bubble, every frame. The second is what the
    /// same frame costs against [`BubbleText`], which the timeline resolved
    /// when it built the rows. `cargo test -p oxidezap-gui -- --ignored
    /// --nocapture per_frame_text_costs`
    #[test]
    #[ignore = "a measurement, not an assertion"]
    fn per_frame_text_costs() {
        use super::BubbleText;

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
            let parsing = started.elapsed();

            // What the rows hold, resolved once — and then handed to a bubble
            // the way the list hands it, which is a clone per visible row.
            let parsed: Vec<BubbleText> = (0..BUBBLES).map(|_| BubbleText::of(source)).collect();
            let started = wacore::time::Instant::now();
            let mut held = 0;
            for _ in 0..FRAMES {
                for text in &parsed {
                    held += std::hint::black_box(text.clone()).runs.len();
                }
            }
            let handing = started.elapsed();

            println!(
                "{what}: {BUBBLES} bubbles x {FRAMES} frames: parsing {parsing:?} \
                 ({:?} per frame, {runs} runs) -> handing out {handing:?} ({:?} per frame, \
                 {held} runs)",
                parsing / FRAMES as u32,
                handing / FRAMES as u32,
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
