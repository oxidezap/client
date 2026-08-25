//! WhatsApp's message markup, as spans.
//!
//! `*bold*`, `_italic_`, `~strikethrough~`, `` `code` `` and ```` ```block```` ````
//! are formatting everywhere WhatsApp is read — a phone renders them and shows
//! the reader the *effect*. A client that prints the markers instead is
//! showing its working, and the difference is most obvious in a code snippet,
//! where the backticks are the loudest thing on screen.
//!
//! Parsed here rather than in the renderer because it is a property of the
//! text, not of a bubble: the same rules apply to a quote's preview line and
//! to anything else that shows message content.
//!
//! The rules are WhatsApp's, and they are deliberately narrow:
//!
//! - A marker only opens when it is followed by something that is not a space,
//!   and only closes when it is preceded by something that is not a space.
//!   `2 * 3 * 4` is arithmetic, not bold.
//! - A run has to close on the same line. An unmatched marker is text.
//! - Inside a code span nothing else is markup, which is the whole point of a
//!   code span.
//! - Styles otherwise nest: `*_both_*` is bold *and* italic.

use std::borrow::Cow;
use std::ops::Range;

/// What a span of text looks like. A set, not a choice: WhatsApp lets bold and
/// italic overlap, so a span can carry both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Emphasis {
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    /// Monospace, from either backtick form.
    pub code: bool,
}

impl Emphasis {
    pub fn is_plain(self) -> bool {
        self == Self::default()
    }

    /// Everything either one asks for. Nesting accumulates rather than
    /// replaces: the inner run of `*_both_*` is both.
    fn union(self, other: Self) -> Self {
        Self {
            bold: self.bold || other.bold,
            italic: self.italic || other.italic,
            strikethrough: self.strikethrough || other.strikethrough,
            code: self.code || other.code,
        }
    }

    fn with(self, marker: char) -> Self {
        match marker {
            '*' => Self { bold: true, ..self },
            '_' => Self {
                italic: true,
                ..self
            },
            '~' => Self {
                strikethrough: true,
                ..self
            },
            _ => Self { code: true, ..self },
        }
    }
}

/// A run of text that shares one appearance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// Byte range into [`RichText::text`].
    pub range: Range<usize>,
    pub emphasis: Emphasis,
}

/// Message text with its markers removed and its emphasis recorded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RichText {
    /// What the reader sees: the source without the markup characters.
    pub text: String,
    /// The runs that are not plain, in order. Empty for ordinary text, which
    /// is the common case and costs one scan and no allocation beyond the
    /// string itself.
    pub spans: Vec<Span>,
}

impl RichText {
    /// Whether anything at all is formatted, so a renderer can take the plain
    /// path without walking the spans.
    pub fn is_plain(&self) -> bool {
        self.spans.is_empty()
    }

    /// The spans flattened into what a text renderer can consume: in order,
    /// non-overlapping, and each carrying every emphasis that covers it.
    ///
    /// [`parse`] emits runs as they *close*, so `*_both_*` produces the inner
    /// italic run nested inside the outer bold one. A text engine styles a
    /// string by walking disjoint runs left to right, so the nesting has to be
    /// resolved into a partition first — and it is resolved here rather than
    /// in a renderer because every renderer would need the same answer.
    ///
    /// Empty for plain text, which is the common case and the reason a caller
    /// should check [`is_plain`](Self::is_plain) before asking.
    pub fn runs(&self) -> Vec<(Range<usize>, Emphasis)> {
        if self.spans.is_empty() {
            return Vec::new();
        }
        // Every span edge is a point where the appearance can change; between
        // two adjacent edges it cannot, so each gap is one run.
        let mut edges = Vec::with_capacity(self.spans.len() * 2);
        for span in &self.spans {
            edges.push(span.range.start);
            edges.push(span.range.end);
        }
        edges.sort_unstable();
        edges.dedup();

        let mut runs: Vec<(Range<usize>, Emphasis)> = Vec::with_capacity(edges.len());
        for pair in edges.windows(2) {
            let (from, to) = (pair[0], pair[1]);
            let emphasis = self
                .spans
                .iter()
                .filter(|span| span.range.start <= from && to <= span.range.end)
                .fold(Emphasis::default(), |acc, span| acc.union(span.emphasis));
            if emphasis.is_plain() {
                continue;
            }
            // A gap only exists because some *other* span ended here; when the
            // appearance did not actually change, extend rather than split.
            match runs.last_mut() {
                Some((range, last)) if range.end == from && *last == emphasis => range.end = to,
                _ => runs.push((from..to, emphasis)),
            }
        }
        runs
    }
}

/// The markers, longest first: ``` has to be tried before `.
const FENCE: &str = "```";
const MARKERS: [char; 4] = ['*', '_', '~', '`'];

/// Parse WhatsApp markup.
///
/// Never fails and never loses characters: anything that is not a completed
/// run comes through as the literal text it was.
pub fn parse(source: &str) -> RichText {
    let mut out = RichText {
        text: String::with_capacity(source.len()),
        spans: Vec::new(),
    };
    // Open runs, innermost last: (marker, index into `out.text` where it began).
    let mut open: Vec<(char, usize)> = Vec::new();
    let bytes = source.as_bytes();
    let mut at = 0;

    while at < source.len() {
        // A fence is three backticks and behaves like one code marker.
        let (marker, width) = if source[at..].starts_with(FENCE) {
            ('`', FENCE.len())
        } else {
            let ch = source[at..].chars().next().expect("in bounds");
            (ch, ch.len_utf8())
        };

        if MARKERS.contains(&marker) {
            // Inside a code run only a code marker can do anything: that is
            // what makes `*not bold*` inside backticks come out literal.
            let in_code = open.iter().any(|(m, _)| *m == '`');
            let usable = !in_code || marker == '`';

            if usable && let Some(depth) = open.iter().rposition(|(m, _)| *m == marker) {
                // A closer needs a non-space before it, or it is text.
                if closes(bytes, at) {
                    let (_, from) = open.remove(depth);
                    let emphasis = open
                        .iter()
                        .fold(Emphasis::default(), |e, (m, _)| e.with(*m))
                        .with(marker);
                    if from < out.text.len() {
                        out.spans.push(Span {
                            range: from..out.text.len(),
                            emphasis,
                        });
                    }
                    at += width;
                    continue;
                }
            } else if usable && opens(bytes, at + width) {
                open.push((marker, out.text.len()));
                at += width;
                continue;
            }
        }

        // Anything else — including a marker that opened nothing — is text.
        let ch = source[at..].chars().next().expect("in bounds");
        out.text.push(ch);
        at += ch.len_utf8();
    }

    // Runs that never closed were never formatting. Their markers are already
    // gone from `out.text`, so put them back where they stood.
    for (marker, from) in open.into_iter().rev() {
        out.text.insert(from, marker);
        for span in &mut out.spans {
            if span.range.start >= from {
                span.range.start += marker.len_utf8();
                span.range.end += marker.len_utf8();
            } else if span.range.end > from {
                span.range.end += marker.len_utf8();
            }
        }
    }

    // Innermost first is the order a renderer wants to apply them in, and the
    // parser produces them by closing order, which is the same thing.
    out.spans.retain(|span| !span.emphasis.is_plain());
    out
}

/// The same text with its markup removed and nothing recorded.
///
/// For the places that show message text as one unstyled line — a chat row's
/// preview, a quote's summary, a search result — where the markers are noise
/// but there is nowhere to put the emphasis. Borrows when there was no markup
/// at all, which is nearly every preview drawn.
pub fn plain_text(source: &str) -> Cow<'_, str> {
    // A string with no marker byte in it cannot parse to anything but itself,
    // and a chat list redraws every row on every frame.
    if !source
        .bytes()
        .any(|b| matches!(b, b'*' | b'_' | b'~' | b'`'))
    {
        return Cow::Borrowed(source);
    }
    Cow::Owned(parse(source).text)
}

/// Whether a marker at this position can open a run: something follows it, and
/// it is not a space.
fn opens(bytes: &[u8], after: usize) -> bool {
    bytes
        .get(after)
        .is_some_and(|b| !b.is_ascii_whitespace() && *b != b'\n')
}

/// Whether a marker at this position can close one: something precedes it, and
/// it is not a space.
fn closes(bytes: &[u8], at: usize) -> bool {
    at > 0 && bytes.get(at - 1).is_some_and(|b| !b.is_ascii_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn only(source: &str) -> (String, Vec<(Range<usize>, Emphasis)>) {
        let rich = parse(source);
        let spans = rich
            .spans
            .iter()
            .map(|s| (s.range.clone(), s.emphasis))
            .collect();
        (rich.text, spans)
    }

    fn bold() -> Emphasis {
        Emphasis {
            bold: true,
            ..Default::default()
        }
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        let rich = parse("hello there");
        assert_eq!(rich.text, "hello there");
        assert!(rich.is_plain());
    }

    #[test]
    fn the_markers_are_removed_and_the_effect_recorded() {
        let (text, spans) = only("say *this* now");
        assert_eq!(text, "say this now");
        assert_eq!(spans, vec![(4..8, bold())]);
    }

    #[test]
    fn each_marker_has_its_own_meaning() {
        for (source, expected) in [
            (
                "*b*",
                Emphasis {
                    bold: true,
                    ..Default::default()
                },
            ),
            (
                "_i_",
                Emphasis {
                    italic: true,
                    ..Default::default()
                },
            ),
            (
                "~s~",
                Emphasis {
                    strikethrough: true,
                    ..Default::default()
                },
            ),
            (
                "`c`",
                Emphasis {
                    code: true,
                    ..Default::default()
                },
            ),
        ] {
            let (text, spans) = only(source);
            assert_eq!(text, source.trim_matches(|c| "*_~`".contains(c)));
            assert_eq!(spans, vec![(0..1, expected)], "for {source}");
        }
    }

    #[test]
    fn a_fence_is_the_same_as_a_backtick() {
        let (text, spans) = only("```fn main()```");
        assert_eq!(text, "fn main()");
        assert_eq!(spans.len(), 1);
        assert!(spans[0].1.code);
    }

    /// The case that made this worth writing: a snippet's punctuation is not
    /// markup, and printing it as bold would be worse than printing the
    /// backticks.
    #[test]
    fn nothing_inside_a_code_run_is_markup() {
        let (text, spans) = only("`a *b* c`");
        assert_eq!(text, "a *b* c");
        assert_eq!(spans.len(), 1);
        assert!(spans[0].1.code && !spans[0].1.bold);
    }

    #[test]
    fn styles_nest() {
        let (text, spans) = only("*_both_*");
        assert_eq!(text, "both");
        assert!(spans.iter().any(|(_, e)| e.bold && e.italic));
    }

    /// `2 * 3 * 4` is arithmetic. A marker with a space after it opens
    /// nothing, and one with a space before it closes nothing.
    #[test]
    fn spaced_markers_are_arithmetic() {
        let rich = parse("2 * 3 * 4");
        assert_eq!(rich.text, "2 * 3 * 4");
        assert!(rich.is_plain());
    }

    #[test]
    fn an_unclosed_marker_is_just_text() {
        let rich = parse("*not bold");
        assert_eq!(rich.text, "*not bold");
        assert!(rich.is_plain());

        let rich = parse("a * b");
        assert_eq!(rich.text, "a * b");
    }

    /// An unclosed marker after a closed one must not shift the closed run off
    /// the text it describes.
    #[test]
    fn an_unclosed_marker_does_not_move_the_runs_before_it() {
        let (text, spans) = only("*bold* then _dangling");
        assert_eq!(text, "bold then _dangling");
        assert_eq!(spans, vec![(0..4, bold())]);
        assert_eq!(&text[0..4], "bold");
    }

    #[test]
    fn no_characters_are_ever_lost() {
        for source in [
            "",
            "*",
            "``",
            "```",
            "*_~`",
            "a*b_c~d`e",
            "*a* _b_ ~c~ `d`",
            "emoji 🎉 *bold 🎉* end",
        ] {
            let rich = parse(source);
            let markers: usize = source.matches(['*', '_', '~', '`']).count();
            assert!(
                rich.text.chars().count() + markers >= source.chars().count(),
                "lost text from {source:?}: {:?}",
                rich.text
            );
        }
    }

    /// A text engine walks runs left to right, so nesting has to come out as
    /// a partition: in order, touching but never overlapping.
    #[test]
    fn runs_are_ordered_and_disjoint() {
        let rich = parse("*bold _and italic_ again* plain `code`");
        let runs = rich.runs();
        assert!(!runs.is_empty());
        let mut last_end = 0;
        for (range, _) in &runs {
            assert!(range.start >= last_end, "runs overlap: {runs:?}");
            assert!(range.start < range.end, "empty run: {runs:?}");
            assert!(rich.text.is_char_boundary(range.start));
            assert!(rich.text.is_char_boundary(range.end));
            last_end = range.end;
        }
        assert!(last_end <= rich.text.len());
    }

    /// The inner run of a nested pair carries both, which is the whole reason
    /// flattening cannot just take the innermost span.
    #[test]
    fn a_nested_run_carries_every_emphasis_over_it() {
        let rich = parse("*bold _both_ bold*");
        let both = rich
            .runs()
            .into_iter()
            .find(|(range, _)| &rich.text[range.clone()] == "both")
            .expect("the nested word is its own run");
        assert!(both.1.bold && both.1.italic);

        for (range, emphasis) in rich.runs() {
            if rich.text[range].contains("bold ") {
                assert!(emphasis.bold && !emphasis.italic);
            }
        }
    }

    /// Plain text asks for nothing, so the renderer can hand the string
    /// straight to the layout engine.
    #[test]
    fn plain_text_has_no_runs() {
        assert!(parse("nothing to see").runs().is_empty());
    }

    /// Two adjacent stretches that look the same are one run: an edge exists
    /// because *some* span ended there, not because the appearance changed.
    #[test]
    fn identical_neighbours_are_not_split() {
        let rich = parse("*a `b` c*");
        let bold_only: Vec<_> = rich
            .runs()
            .into_iter()
            .filter(|(_, e)| e.bold && !e.code)
            .collect();
        assert_eq!(
            bold_only.len(),
            2,
            "one run each side of the code: {bold_only:?}"
        );
    }

    /// A preview has nowhere to put emphasis, so it gets the text alone — and
    /// text that was never marked up is not copied to say so.
    #[test]
    fn plain_text_strips_markup_and_borrows_when_there_is_none() {
        assert!(matches!(plain_text("nothing here"), Cow::Borrowed(_)));
        assert_eq!(plain_text("say *this* now"), "say this now");
        assert_eq!(plain_text("`code`"), "code");
        assert_eq!(plain_text("2 * 3"), "2 * 3");
    }

    #[test]
    fn spans_land_on_character_boundaries() {
        let rich = parse("*🎉 party* over");
        assert_eq!(rich.text, "🎉 party over");
        for span in &rich.spans {
            assert!(
                rich.text.is_char_boundary(span.range.start)
                    && rich.text.is_char_boundary(span.range.end),
                "span {span:?} splits a character"
            );
        }
    }
}
