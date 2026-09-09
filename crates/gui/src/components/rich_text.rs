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
    App, FontStyle, FontWeight, HighlightStyle, InteractiveText, IntoElement, SharedString,
    StrikethroughStyle, StyledText, UnderlineStyle,
};
use gpui_component::ActiveTheme as _;

use crate::theme::ActiveProductTheme as _;

use oxidezap_core::{Emphasis, LinkSpan, find_links_in, parse_rich_text};

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
    /// What each link opens, shared the same way. Resolved once, here,
    /// rather than once per repaint: turning every target `String` into a
    /// `SharedString` on every frame copied peer-controlled text with the
    /// message length, over and over, for no new information.
    link_targets: Arc<[SharedString]>,
}

impl BubbleText {
    /// Parse `source` once, for the timeline that will draw it many times.
    pub fn of(source: &str) -> Self {
        let rich = parse_rich_text(source);
        // Against the parsed text, not the source: the ranges describe what
        // the reader sees, and the span edges count as boundaries where a
        // removed delimiter stood — see `find_links_in` — so an address
        // starting where formatting ends is still found.
        let links: Arc<[LinkSpan]> = find_links_in(&rich).into();
        let link_targets: Arc<[SharedString]> = links
            .iter()
            .map(|link| SharedString::from(link.target.as_str()))
            .collect();
        if rich.is_plain() {
            return Self {
                text: rich.text.into(),
                runs: Arc::from([]),
                links,
                link_targets,
            };
        }
        let runs = rich.runs();
        Self {
            text: rich.text.into(),
            runs: runs.into(),
            links,
            link_targets,
        }
    }

    /// Whether there is anything to draw. The bubble asks before it builds a
    /// text box at all, because a media message routinely has no caption.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// What each link opens, in link order. The message menu lists these, so
    /// every address has an equivalent activation route beside the inline
    /// pointer one.
    pub fn link_targets(&self) -> &Arc<[SharedString]> {
        &self.link_targets
    }
}

/// Message text with its markup applied.
///
/// Returns an element either way: the caller styles size and colour on the
/// parent, and both paths inherit it.
pub fn render_rich_text(parsed: &BubbleText, cx: &App) -> gpui::AnyElement {
    if !parsed.links.is_empty() {
        return render_with_links(parsed, cx).into_any_element();
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

/// Message text that holds links, in one inline flow.
///
/// A `StyledText` paints but answers no clicks, so the addresses ride along
/// as clickable ranges on an `InteractiveText` instead of becoming elements
/// of their own. Nothing is split into flex children, so a newline before an
/// address starts a line the way it does without one, and a long address
/// wraps the way plain text does rather than overflowing its item into the
/// bubble's `overflow_hidden`. Clicks open the target through `cx.open_url`,
/// which is why this needs no platform split of its own: GPUI answers that
/// on the desktop and in the page alike. Size and colour are inherited from
/// the parent; only the link ink comes from the theme.
fn render_with_links(parsed: &BubbleText, cx: &App) -> impl IntoElement + use<> {
    let metrics = cx.product().metrics;
    let ink = cx.theme().link;
    let mono = cx.theme().mono_font_family.clone();
    let text: SharedString = parsed.text.clone();
    // Every emphasis edge and every link edge is a point where the
    // appearance can change; between two adjacent edges it cannot, so each
    // gap is one run. Both partitions arrive sorted and disjoint, which is
    // what keeps the highlights handed to `StyledText` disjoint too: two
    // highlights for one byte is a panic, not a blend.
    let mut edges = Vec::with_capacity(parsed.runs.len() * 2 + parsed.links.len() * 2 + 2);
    edges.push(0);
    edges.push(text.len());
    for (range, _) in parsed.runs.iter() {
        edges.push(range.start);
        edges.push(range.end);
    }
    for link in parsed.links.iter() {
        edges.push(link.range.start);
        edges.push(link.range.end);
    }
    edges.sort_unstable();
    edges.dedup();
    let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let mut code: Vec<(Range<usize>, SharedString)> = Vec::new();
    let mut run_ix = 0;
    let mut link_ix = 0;
    for cell in edges.windows(2) {
        let (start, end) = (cell[0], cell[1]);
        if start == end {
            continue;
        }
        while run_ix < parsed.runs.len() && parsed.runs[run_ix].0.end <= start {
            run_ix += 1;
        }
        while link_ix < parsed.links.len() && parsed.links[link_ix].range.end <= start {
            link_ix += 1;
        }
        let emphasis = match parsed.runs.get(run_ix) {
            Some((range, emphasis)) if range.start < end && range.end > start => *emphasis,
            _ => Emphasis::default(),
        };
        let linked = parsed
            .links
            .get(link_ix)
            .is_some_and(|link| link.range.start <= start && start < link.range.end);
        if emphasis.is_plain() && !linked {
            continue;
        }
        if linked {
            highlights.push((start..end, link_style(emphasis, metrics, ink)));
        } else {
            highlights.push((start..end, style_for(emphasis, metrics)));
        }
        if emphasis.code {
            code.push((start..end, mono.clone()));
        }
    }
    let styled = StyledText::new(text)
        .with_highlights(highlights)
        .with_font_family_overrides(code);
    let ranges: Vec<Range<usize>> = parsed.links.iter().map(|link| link.range.clone()).collect();
    // A refcount per frame, not a copy per target: the strings were shared
    // when the bubble was parsed.
    let targets = parsed.link_targets.clone();
    // One instance per bubble, scoped under the row's own id.
    InteractiveText::new("message-links", styled).on_click(ranges, move |ix, _window, cx| {
        cx.open_url(&targets[ix]);
    })
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

    /// Markup characters inside an address are address characters, not
    /// formatting: the parser leaves them alone, so the target is exactly
    /// what the sender wrote rather than the address with pieces missing.
    #[test]
    fn link_targets_keep_markup_characters_inside_the_address() {
        let parsed = BubbleText::of("see https://example.com/foo_bar_baz now");
        assert_eq!(parsed.links.len(), 1);
        assert_eq!(
            &parsed.text[parsed.links[0].range.clone()],
            "https://example.com/foo_bar_baz"
        );
        assert_eq!(parsed.links[0].target, "https://example.com/foo_bar_baz");
    }

    /// A marker around an address styles the address without joining it:
    /// the target is exactly what the sender wrote.
    #[test]
    fn markup_around_a_link_styles_it_and_keeps_the_target() {
        for (source, flag) in [
            ("*https://example.com*", "bold"),
            ("_https://example.com_", "italic"),
            ("~https://example.com~", "strike"),
            ("`https://example.com`", "code"),
        ] {
            let parsed = BubbleText::of(source);
            assert_eq!(parsed.text, "https://example.com", "for {source:?}");
            assert_eq!(parsed.links.len(), 1, "for {source:?}");
            assert_eq!(
                &parsed.text[parsed.links[0].range.clone()],
                "https://example.com",
                "for {source:?}"
            );
            assert_eq!(
                parsed.links[0].target, "https://example.com",
                "for {source:?}"
            );
            assert_eq!(parsed.runs.len(), 1, "for {source:?}: {:?}", parsed.runs);
            let emphasis = parsed.runs[0].1;
            assert!(
                match flag {
                    "bold" => emphasis.bold,
                    "italic" => emphasis.italic,
                    "strike" => emphasis.strikethrough,
                    _ => emphasis.code,
                },
                "for {source:?}: {emphasis:?}"
            );
        }
    }

    /// The wrapped address keeps the markers inside it: they are address
    /// characters, not formatting.
    #[test]
    fn a_wrapped_link_keeps_markers_inside_the_address() {
        let parsed = BubbleText::of("*https://example.com/foo_bar_baz*");
        assert_eq!(parsed.text, "https://example.com/foo_bar_baz");
        assert_eq!(parsed.links.len(), 1);
        assert_eq!(parsed.links[0].target, "https://example.com/foo_bar_baz");
        assert_eq!(parsed.runs.len(), 1);
        assert!(parsed.runs[0].1.bold);
    }

    /// A newline ends the line even when an address follows it: detection
    /// runs over the whole text, and the single inline flow draws it.
    #[test]
    fn a_link_after_a_newline_is_still_one_link() {
        let parsed = BubbleText::of("before\nhttps://example.com");
        assert_eq!(parsed.links.len(), 1);
        assert_eq!(
            &parsed.text[parsed.links[0].range.clone()],
            "https://example.com"
        );
        assert_eq!(parsed.links[0].target, "https://example.com");
    }

    /// Removing the closing marker joins the label to the address, where it
    /// reads as the middle of a word. The span edge marks where the
    /// delimiter stood, so detection still sees the boundary and the
    /// address stays clickable.
    #[test]
    fn a_link_right_after_formatted_text_is_still_a_link() {
        let parsed = BubbleText::of("*label*https://example.com");
        assert_eq!(parsed.text, "labelhttps://example.com");
        assert_eq!(parsed.links.len(), 1);
        assert_eq!(
            &parsed.text[parsed.links[0].range.clone()],
            "https://example.com"
        );
        assert_eq!(parsed.links[0].target, "https://example.com");
        assert_eq!(parsed.link_targets().len(), 1);
        assert_eq!(parsed.link_targets()[0], "https://example.com");
    }

    /// A closer followed by text ends the address at the closer: the marker
    /// must not leak into the target the click opens.
    #[test]
    fn a_link_closer_followed_by_text_does_not_join_the_target() {
        let parsed = BubbleText::of("*https://example.com*x");
        assert_eq!(parsed.text, "https://example.comx");
        assert_eq!(parsed.links.len(), 1);
        assert_eq!(
            &parsed.text[parsed.links[0].range.clone()],
            "https://example.com"
        );
        assert_eq!(parsed.links[0].target, "https://example.com");
        assert!(!parsed.links[0].target.contains('*'));
        assert_eq!(parsed.runs.len(), 1);
        assert!(parsed.runs[0].1.bold);
    }

    /// The shared targets travel with the parsed links: one entry per link,
    /// in order, holding exactly what the click opens. The repaint path
    /// clones this `Arc`, never the strings, so this is also what the menu
    /// lists without rescanning.
    #[test]
    fn link_targets_are_shared_once_per_bubble() {
        let plain = BubbleText::of("nothing to open here");
        assert!(plain.link_targets().is_empty());

        let parsed = BubbleText::of("see https://one.example/x and http://two.example/y");
        assert_eq!(parsed.links.len(), 2);
        let targets = parsed.link_targets();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0], "https://one.example/x");
        assert_eq!(targets[1], "http://two.example/y");
        assert_eq!(targets[0].as_str(), parsed.links[0].target.as_str());
        assert_eq!(targets[1].as_str(), parsed.links[1].target.as_str());
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
