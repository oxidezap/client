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

    /// The four attributes in a fixed order, so the sweep in `runs` can keep
    /// a count of each: nesting accumulates rather than replaces, and the
    /// inner run of `*_both_*` is both.
    fn attributes(self) -> [bool; 4] {
        [self.bold, self.italic, self.strikethrough, self.code]
    }

    fn from_attributes([bold, italic, strikethrough, code]: [bool; 4]) -> Self {
        Self {
            bold,
            italic,
            strikethrough,
            code,
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
        // two adjacent edges it cannot, so each gap is one run. Which
        // emphasis is in force is carried across the sweep rather than looked
        // up: asking every span about every gap is quadratic, and the text is
        // the peer's, unbounded, and re-run on every repaint of every visible
        // bubble.
        let mut events: Vec<(usize, bool, Emphasis)> = Vec::with_capacity(self.spans.len() * 2);
        for span in &self.spans {
            events.push((span.range.start, false, span.emphasis));
            events.push((span.range.end, true, span.emphasis));
        }
        // Closings first at a shared point, so a span that ends where the next
        // begins is not counted through it.
        events.sort_unstable_by_key(|(at, closing, _)| (*at, std::cmp::Reverse(*closing)));

        // A count per attribute rather than a set of spans: the same
        // attribute can be in force from several of them at once.
        let mut depth = [0usize; 4];
        let mut runs: Vec<(Range<usize>, Emphasis)> = Vec::with_capacity(events.len());
        let mut at = 0;
        while at < events.len() {
            let from = events[at].0;
            while at < events.len() && events[at].0 == from {
                let (_, closing, emphasis) = events[at];
                for (count, on) in depth.iter_mut().zip(emphasis.attributes()) {
                    if on {
                        *count = if closing { *count - 1 } else { *count + 1 };
                    }
                }
                at += 1;
            }
            let Some(&(to, ..)) = events.get(at) else {
                break;
            };
            let emphasis = Emphasis::from_attributes(depth.map(|count| count > 0));
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
    let mut open: Vec<Open> = Vec::new();
    let bytes = source.as_bytes();
    let mut at = 0;

    while at < source.len() {
        // A fence is three backticks and means the same thing as one, but it
        // is not delimited the same way — see `Open::fenced`.
        let (marker, fenced, width) = if source[at..].starts_with(FENCE) {
            ('`', true, FENCE.len())
        } else {
            let ch = source[at..].chars().next().expect("in bounds");
            (ch, false, ch.len_utf8())
        };

        if MARKERS.contains(&marker) {
            // Inside a code run only a code marker can do anything: that is
            // what makes `*not bold*` inside backticks come out literal.
            let in_code = open.iter().any(|o| o.marker == '`');
            let usable = !in_code || marker == '`';

            // A fence closes a fence and a bare backtick closes a bare one.
            // Matching them to each other would let the first backtick of a
            // closing fence end the run and leave two behind as text.
            let closing = open
                .iter()
                .rposition(|o| o.marker == marker && o.fenced == fenced);

            if usable && let Some(depth) = closing {
                if fenced || closes(bytes, at) {
                    // Nothing between the two delimiters. That was never a
                    // run, and consuming the closer anyway deleted characters
                    // the sender typed: `*_*` came out as `_`. The opener
                    // stays on the stack, where the unclosed pass puts it
                    // back, and this one is text.
                    if open[depth].from == out.text.len() {
                        out.text.push_str(open[depth].literal());
                        at += width;
                        continue;
                    }
                    // A marker still open *inside* this run never matched
                    // anything, so it is text — and text lends no emphasis to
                    // the run closing over it. Left on the stack it did:
                    // `*bold _dangling*` came out bold *and* italic, with the
                    // stray underscore restored inside the span it had no
                    // business styling.
                    restore_unclosed(&mut out, &mut open, depth + 1);
                    let from = open.remove(depth).from;
                    // The newline that put the closing fence on its own line
                    // belongs to the delimiter, not to the code. Left in, every
                    // block ended on a blank line the sender never typed.
                    if fenced && out.text.len() > from && out.text.ends_with('\n') {
                        out.text.pop();
                    }
                    let emphasis = open
                        .iter()
                        .fold(Emphasis::default(), |e, o| e.with(o.marker))
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
            } else if usable && (fenced || opens(bytes, at + width)) {
                open.push(Open {
                    marker,
                    fenced,
                    from: out.text.len(),
                });
                at += width;
                // Symmetrically: the newline that ends the opening fence's
                // line is part of writing a fence, not the first character of
                // the snippet. Only one, so a deliberate blank line survives.
                if fenced && bytes.get(at) == Some(&b'\n') {
                    at += 1;
                }
                continue;
            }
        }

        // Anything else — including a marker that opened nothing — is text.
        let ch = source[at..].chars().next().expect("in bounds");
        // Where the one-line rule is enforced. A `*` that never met its
        // closer on its own line is a character somebody typed, not the
        // start of a run: without this, a marker in a paragraph further down
        // reached back and emphasised everything in between. A fence is the
        // exception it exists for — it is the one form that spans lines — so
        // while one is open the newline is nothing but text.
        if ch == '\n' && !open.iter().any(|run| run.fenced) {
            restore_unclosed(&mut out, &mut open, 0);
        }
        out.text.push(ch);
        at += ch.len_utf8();
    }

    restore_unclosed(&mut out, &mut open, 0);

    // Innermost first is the order a renderer wants to apply them in, and the
    // parser produces them by closing order, which is the same thing.
    out.spans.retain(|span| !span.emphasis.is_plain());
    out
}

/// Put back the delimiters of runs that never closed.
///
/// They were never formatting, and their markers are already gone from the
/// text, so they go back where they stood — all of a fence's three
/// characters, not one of them, or the text loses two and every span after
/// the hole describes the wrong letters.
fn restore_unclosed(out: &mut RichText, open: &mut Vec<Open>, from_depth: usize) {
    for run in open.drain(from_depth..).rev() {
        let literal = run.literal();
        out.text.insert_str(run.from, literal);
        let shift = literal.len();
        for span in &mut out.spans {
            if span.range.start >= run.from {
                span.range.start += shift;
                span.range.end += shift;
            } else if span.range.end > run.from {
                span.range.end += shift;
            }
        }
    }
}

/// A run whose opening delimiter has been seen and whose closer has not.
struct Open {
    marker: char,
    /// Whether the delimiter was ```` ``` ````.
    ///
    /// Which is not a detail of how it was written. A one-character marker is
    /// delimited by word adjacency — `*` only opens before a non-space and
    /// only closes after one, which is what keeps `2 * 3 * 4` arithmetic. A
    /// fence carries no such ambiguity: it is three characters nothing types
    /// by accident, and a fenced block is normally opened at the end of a line
    /// and closed at the start of one. Holding it to the adjacency rule meant
    /// the newline on either side stopped it from ever opening or ever
    /// closing, so the one form WhatsApp exists to show code in was the one
    /// form that came out as literal backticks.
    fenced: bool,
    /// Where the run began, as an index into the parsed text.
    from: usize,
}

impl Open {
    /// What this delimiter was written as, for putting back.
    fn literal(&self) -> &'static str {
        if self.fenced {
            FENCE
        } else {
            match self.marker {
                '*' => "*",
                '_' => "_",
                '~' => "~",
                _ => "`",
            }
        }
    }
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

    /// The form WhatsApp exists to show code in, and the one that was broken:
    /// a fenced block is opened at the end of a line and closed at the start
    /// of one, so the newline on either side is exactly where the adjacency
    /// rule for one-character markers would refuse it.
    #[test]
    fn a_fenced_block_spans_lines() {
        let (text, spans) = only("```\nfn main() {}\n```");
        assert_eq!(
            text, "fn main() {}",
            "the fence's own newlines are not code"
        );
        assert_eq!(spans.len(), 1, "one code run: {spans:?}");
        assert!(spans[0].1.code);
    }

    /// The one-line rule, which the module has always documented and the
    /// parser did not enforce: a `*` that finds no closer before the end of
    /// its line is a character somebody typed. Without this a marker in a
    /// later paragraph reached back and emphasised everything between them.
    #[test]
    fn an_ordinary_run_closes_on_its_own_line() {
        let rich = parse("*first\nsecond*");
        assert_eq!(rich.text, "*first\nsecond*");
        assert!(rich.is_plain(), "{:?}", rich.spans);

        // Two lines, each closing its own run, is still two runs.
        let (text, spans) = only("*first*\n*second*");
        assert_eq!(text, "first\nsecond");
        assert_eq!(spans, vec![(0..5, bold()), (6..12, bold())]);
    }

    /// A marker left open inside a run was never formatting, so it must not
    /// lend its emphasis to the run that closes over it. `*bold _dangling*`
    /// is bold, with an underscore in it — not bold *and* italic.
    #[test]
    fn an_unmatched_inner_marker_styles_nothing() {
        let (text, spans) = only("*bold _dangling*");
        assert_eq!(text, "bold _dangling");
        assert_eq!(spans.len(), 1, "one run: {spans:?}");
        assert!(spans[0].1.bold);
        assert!(!spans[0].1.italic, "the stray underscore is text");
        assert_eq!(&text[spans[0].0.clone()], "bold _dangling");
    }

    /// The exception the fence exists for: inside one, a newline is text and
    /// the run stays open across it.
    #[test]
    fn a_newline_does_not_close_a_fence() {
        let (text, spans) = only("```\nfirst\nsecond\n```");
        assert_eq!(text, "first\nsecond");
        assert_eq!(spans.len(), 1, "one code run: {spans:?}");
        assert!(spans[0].1.code);
    }

    /// A fence that never closes is text, and it is three characters of text.
    /// Putting one back lost two and slid every span after the hole onto the
    /// wrong letters.
    #[test]
    fn an_unclosed_fence_comes_back_whole() {
        let rich = parse("```fn main()");
        assert_eq!(rich.text, "```fn main()");
        assert!(rich.is_plain());

        let (text, spans) = only("*bold* then ```dangling");
        assert_eq!(text, "bold then ```dangling");
        assert_eq!(spans, vec![(0..4, bold())]);
        assert_eq!(&text[0..4], "bold");
    }

    /// The closing fence is three characters. Letting its first backtick end
    /// the run left the other two sitting in the text as literal noise.
    #[test]
    fn a_fence_is_not_closed_by_a_bare_backtick() {
        let (text, _) = only("```code```");
        assert_eq!(text, "code");
    }

    /// Nothing inside a fence is markup either — that is what a code block is
    /// for, and the reason a snippet was worth parsing in the first place.
    #[test]
    fn nothing_inside_a_fence_is_markup() {
        let (text, spans) = only("```let x = *y * z*;```");
        assert_eq!(text, "let x = *y * z*;");
        assert_eq!(spans.len(), 1);
        assert!(spans[0].1.code && !spans[0].1.bold);
    }

    /// A closer that immediately follows its opener closes nothing. Consuming
    /// it anyway deleted characters the sender typed, and the reader saw text
    /// that was never written.
    #[test]
    fn an_empty_run_is_two_literal_markers() {
        for (source, expected) in [
            ("*_*", "*_*"),
            ("hi ** there", "hi ** there"),
            ("``", "``"),
            ("``````", "``````"),
        ] {
            let rich = parse(source);
            assert_eq!(rich.text, expected, "for {source:?}");
            assert!(rich.is_plain(), "for {source:?}: {:?}", rich.spans);
        }
    }

    #[test]
    fn no_characters_are_ever_lost() {
        for source in [
            "",
            "*",
            "``",
            "```",
            "````",
            "```\n",
            "*_~`",
            "a*b_c~d`e",
            "*a* _b_ ~c~ `d`",
            "```\ncode\n```",
            "emoji 🎉 *bold 🎉* end",
        ] {
            let rich = parse(source);
            // Delimiters are consumed on purpose — the markers themselves, and
            // the newline that puts a fence on its own line. Everything else a
            // sender typed has to come out the other side, in order.
            let kept = |text: &str| -> String {
                text.chars()
                    .filter(|c| !matches!(c, '*' | '_' | '~' | '`' | '\n'))
                    .collect()
            };
            assert_eq!(
                kept(&rich.text),
                kept(source),
                "characters went missing from {source:?}: {:?}",
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

    /// What one received message can cost the window.
    ///
    /// A stopwatch rather than an assertion, so it is ignored by default. The
    /// old `runs` asked every span about every gap between edges, and
    /// `render_rich_text` calls it on every repaint of every visible bubble:
    /// a message the peer can simply send made repainting the conversation
    /// take longer than a frame, by a lot.
    ///
    /// `cargo test -p oxidezap-core -- --ignored runs_cost`
    #[test]
    #[ignore = "a stopwatch, not a test"]
    fn runs_cost() {
        for spans in [500, 1_000, 2_000, 4_000, 8_000, 16_000] {
            let text = "*a* ".repeat(spans);
            let rich = parse(&text);
            assert_eq!(rich.spans.len(), spans);
            let started = wacore::time::Instant::now();
            let runs = rich.runs();
            println!(
                "{spans} spans -> {} runs in {:?}",
                runs.len(),
                started.elapsed()
            );
        }
    }
}
