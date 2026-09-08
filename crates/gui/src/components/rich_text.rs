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
    App, FontStyle, FontWeight, HighlightStyle, IntoElement, ParentElement, SharedString,
    StrikethroughStyle, Styled, StyledText, UnderlineStyle, div,
};
use gpui_component::ActiveTheme as _;
use gpui_component::link::Link;

use crate::theme::ActiveProductTheme as _;

use oxidezap_core::{Emphasis, LinkSpan, find_message_links, parse_rich_text};

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
    /// The URLs in the parsed text, in order. Empty for text without any —
    /// which is the common case, and the case that then costs one refcount
    /// per frame and nothing else.
    links: Arc<[LinkSpan]>,
}

impl BubbleText {
    /// Parse `source` once, for the timeline that will draw it many times.
    pub fn of(source: &str) -> Self {
        let rich = parse_rich_text(source);
        let links: Arc<[LinkSpan]> = find_message_links(&rich.text).into();
        if rich.is_plain() {
            return Self {
                text: rich.text.into(),
                runs: Arc::from([]),
                links,
            };
        }
        let runs = rich.runs();
        Self {
            text: rich.text.into(),
            runs: runs.into(),
            links,
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
    if !parsed.links.is_empty() {
        return render_with_links(parsed, cx);
    }
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

/// Message text that holds links: plain stretches with one `Link` per
/// address, wrapped into the line box the plain path fills.
///
/// A `StyledText` paints but answers no clicks, so an address has to be its
/// own element. `Link` opens its `href` through `cx.open_url`, which is why
/// this needs no platform split of its own: GPUI answers that on the desktop
/// and in the page alike. Size and colour are inherited from the parent; only
/// the link ink comes from the theme.
fn render_with_links(parsed: &BubbleText, cx: &App) -> gpui::AnyElement {
    let mut children = Vec::new();
    let mut at = 0;
    for (ix, link) in parsed.links.iter().enumerate() {
        if at < link.range.start {
            children.push(render_plain_segment(parsed, at..link.range.start, cx));
        }
        children.push(render_link_segment(parsed, link, ix, cx));
        at = link.range.end;
    }
    if at < parsed.text.len() {
        children.push(render_plain_segment(parsed, at..parsed.text.len(), cx));
    }
    div()
        .flex()
        .flex_wrap()
        .children(children)
        .into_any_element()
}

/// One stretch without links, with the emphasis clipped to it. A stretch
/// with nothing to say about any range goes out as a plain string, for the
/// reason the plain path in [`render_rich_text`] does.
fn render_plain_segment(parsed: &BubbleText, range: Range<usize>, cx: &App) -> gpui::AnyElement {
    let metrics = cx.product().metrics;
    let highlights: Vec<(Range<usize>, HighlightStyle)> = clip_runs(&parsed.runs, &range)
        .map(|(range, emphasis)| (range, style_for(emphasis, metrics)))
        .collect();
    let code = code_overrides(&parsed.runs, &range, cx);
    let slice: SharedString = parsed.text[range].to_string().into();
    if highlights.is_empty() && code.is_empty() {
        return slice.into_any_element();
    }
    StyledText::new(slice)
        .with_highlights(highlights)
        .with_font_family_overrides(code)
        .into_any_element()
}

/// One address: a `Link` opening the target, drawn inked and underlined, with
/// the emphasis it overlaps kept — a bold address stays bold.
fn render_link_segment(
    parsed: &BubbleText,
    link: &LinkSpan,
    ix: usize,
    cx: &App,
) -> gpui::AnyElement {
    let metrics = cx.product().metrics;
    let ink = cx.theme().link;
    // The emphasis pieces inside the address, each carrying the link ink and
    // its underline on top of its own weight and slant, and the ink alone
    // over the gaps between them. Together they cover the address end to end
    // with disjoint runs, which is what `StyledText` requires: two highlights
    // for one byte is a panic, not a blend.
    let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let mut at = 0;
    for (range, emphasis) in clip_runs(&parsed.runs, &link.range) {
        if at < range.start {
            highlights.push((
                at..range.start,
                link_style(Emphasis::default(), metrics, ink),
            ));
        }
        highlights.push((range.clone(), link_style(emphasis, metrics, ink)));
        at = range.end;
    }
    if at < link.range.len() {
        highlights.push((
            at..link.range.len(),
            link_style(Emphasis::default(), metrics, ink),
        ));
    }
    let code = code_overrides(&parsed.runs, &link.range, cx);
    let slice: SharedString = parsed.text[link.range.clone()].to_string().into();
    Link::new(ix)
        .href(link.target.clone())
        .child(
            StyledText::new(slice)
                .with_highlights(highlights)
                .with_font_family_overrides(code),
        )
        .into_any_element()
}

/// The emphasis runs overlapping `range`, rebased to its start. Clipping a
/// partition keeps it one — disjoint and ordered — which is the shape both
/// `StyledText` callers above have to hand over.
fn clip_runs<'a>(
    runs: &'a [(Range<usize>, Emphasis)],
    range: &Range<usize>,
) -> impl Iterator<Item = (Range<usize>, Emphasis)> + use<'a> {
    let (start, end) = (range.start, range.end);
    runs.iter()
        .filter(move |(run, _)| run.start < end && run.end > start)
        .map(move |(run, emphasis)| {
            (
                run.start.max(start) - start..run.end.min(end) - start,
                *emphasis,
            )
        })
}

/// The monospace swaps over `range`, resolved against this frame's theme.
fn code_overrides(
    runs: &[(Range<usize>, Emphasis)],
    range: &Range<usize>,
    cx: &App,
) -> Vec<(Range<usize>, SharedString)> {
    let mono = cx.theme().mono_font_family.clone();
    clip_runs(runs, range)
        .filter(|(_, emphasis)| emphasis.code)
        .map(|(range, _)| (range, mono.clone()))
        .collect()
}

/// One run's appearance inside a link: its own emphasis, inked and underlined
/// as a link. The ink is the theme's rather than the bubble's, so an address
/// reads as one on every ground a bubble paints.
fn link_style(
    emphasis: Emphasis,
    metrics: crate::theme::Metrics,
    ink: gpui::Hsla,
) -> HighlightStyle {
    HighlightStyle {
        color: Some(ink),
        underline: Some(UnderlineStyle {
            thickness: metrics.hairline(),
            color: None,
            wavy: false,
        }),
        ..style_for(emphasis, metrics)
    }
}

#[cfg(test)]
mod tests {
    use super::BubbleText;

    /// Links are resolved where the rows are built, beside the markup — so a
    /// frame hands out ranges rather than scanning the peer's text again.
    #[test]
    fn links_are_detected_once_per_bubble() {
        let plain = BubbleText::of("nothing to open here");
        assert!(plain.links.is_empty());

        let linked = BubbleText::of("see https://example.com/x now");
        assert_eq!(linked.links.len(), 1);
        assert_eq!(
            &linked.text[linked.links[0].range.clone()],
            "https://example.com/x"
        );
        assert_eq!(linked.links[0].target, "https://example.com/x");

        let bare = BubbleText::of("try www.example.com/a");
        assert_eq!(bare.links.len(), 1);
        assert_eq!(bare.links[0].target, "https://www.example.com/a");
    }

    /// Markup is stripped before detection runs, so the ranges describe what
    /// the reader sees rather than what the sender typed.
    #[test]
    fn links_line_up_with_the_parsed_text() {
        let parsed = BubbleText::of("*look* at https://example.com/x!");
        assert_eq!(parsed.text, "look at https://example.com/x!");
        assert_eq!(parsed.links.len(), 1);
        assert_eq!(
            &parsed.text[parsed.links[0].range.clone()],
            "https://example.com/x"
        );
    }

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
