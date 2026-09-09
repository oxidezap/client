//! URLs in message text, as spans.
//!
//! A message that prints `https://example.com` as dead text is showing its
//! working the way one that prints `*bold*` does: every other client renders
//! links and opens them. Detection lives here rather than in the renderer for
//! the same reason the markup rules do — it is a property of the text, not of
//! a bubble — and it runs on the *parsed* text, after the markup is gone, so
//! the ranges line up with what the reader sees.
//!
//! Deliberately narrow: `http://`, `https://` and `www.` are links, nothing
//! else is. No new dependency for it either — the scan is one pass over ASCII
//! prefixes, and a crate would be a heavier answer than the dozen lines here.

use std::ops::Range;

/// One URL in the text, and where opening it should go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSpan {
    /// Byte range into the scanned text.
    pub range: Range<usize>,
    /// What to open. The text as written when it names its own scheme, and
    /// `https://` in front when it starts at `www.`, which no opener would
    /// know what to do with bare.
    pub target: String,
}

/// Find the URLs in `text`.
///
/// Never fails and never overlaps: anything that is not a completed link is
/// left for the caller to draw as ordinary text. Trailing sentence
/// punctuation (`.`, `,`, `!`, their CJK equivalents, a trailing quote, an
/// unbalanced `)`) belongs to the sentence, not to the address.
pub fn find_links(text: &str) -> Vec<LinkSpan> {
    find_links_with_boundaries(text, &[])
}

/// Find the URLs in `text`, honouring markup boundaries.
///
/// Marker removals join what the sender separated:
/// `*label*https://example.com` displays as `labelhttps://example.com`,
/// where the address looks like the middle of a word. Each entry of
/// `boundaries` is a display offset where a delimiter stood, and counts the
/// way whitespace does — without them the address is unclickable although
/// the source had a delimiter at its edge. Sorted; empty for text that was
/// never marked up. See [`find_links_in`].
pub fn find_links_with_boundaries(text: &str, boundaries: &[usize]) -> Vec<LinkSpan> {
    let bytes = text.as_bytes();
    let mut links = Vec::new();
    let mut at = 0;
    // Reused across the candidates of one token, for the reason `TokenScan`
    // states: one long token of repeated prefixes probes many candidates.
    let mut scan = TokenScan::default();
    while at < bytes.len() {
        let Some((end, bare)) = link_end_at(text, at, &mut scan, boundaries) else {
            at += 1;
            continue;
        };
        let shown = &text[at..end];
        links.push(LinkSpan {
            range: at..end,
            target: if bare {
                format!("https://{shown}")
            } else {
                shown.to_string()
            },
        });
        at = end;
    }
    links
}

/// Find the URLs in parsed message text.
///
/// The same as [`find_links`] on [`RichText::text`](crate::RichText::text),
/// except the span edges count as boundaries: they are where the markup
/// delimiters stood, so an address starting where a run ends was delimited
/// in the source even though the display joins them. Without this,
/// `*label*https://example.com` parses to `labelhttps://example.com` and
/// the address reads as mid-word.
pub fn find_links_in(rich: &crate::RichText) -> Vec<LinkSpan> {
    let mut boundaries = Vec::with_capacity(rich.spans.len() * 2);
    for span in &rich.spans {
        boundaries.push(span.range.start);
        boundaries.push(span.range.end);
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    find_links_with_boundaries(&rich.text, &boundaries)
}

/// Scan state reused across the candidates of one whitespace-free token.
///
/// Both `find_links` and the markup parser probe every byte, so one token
/// can hold thousands of rejected prefixes — `http:///` repeated is one
/// token of nothing but prefixes. Caching only the token end still leaves
/// the trailing cleanup walking the same closers per candidate, which is
/// quadratic in the peer's message. The end and both balances are computed
/// once per token and then kept current: each new candidate subtracts the
/// bytes it skipped past since the last one, and those skipped gaps
/// partition the token, so the whole token costs one scan no matter how many
/// prefixes it holds. The trailing cleanup is cached the same way: one
/// backward walk per token records the suffix, and each candidate resolves
/// its end from that record plus the current balances. A markup cut is
/// cached a third way: one forward walk per token records the tail past
/// every edge, and each candidate subtracts its own edge's tail rather than
/// walking the unchanged suffix again.
#[derive(Debug, Default)]
pub(crate) struct TokenScan {
    state: Option<TokenState>,
}

#[derive(Debug)]
struct TokenState {
    end: usize,
    rest: usize,
    parens: i32,
    brackets: i32,
    suffix_start: usize,
    paren_stops: Vec<usize>,
    bracket_stops: Vec<usize>,
    /// The tail delimiter balances over `edge..end` for the markup edges
    /// inside this token, sorted by edge. Recorded in one forward walk on
    /// the first cut; see [`TokenScan::cut_balances`].
    cut_tails: Vec<(usize, i32, i32)>,
    cut_ready: bool,
    /// The same trailing-suffix record as `suffix_start` and friends, for
    /// the last markup cut this token answered. A token's candidates share
    /// one cut, so one record serves them all; a new cut scans once and
    /// replaces it.
    cut_suffix: Option<(usize, usize, Vec<usize>, Vec<usize>)>,
}

impl TokenScan {
    /// The token end and the delimiter balances over `rest..end`, kept
    /// current across candidates inside one token.
    fn balances(&mut self, text: &str, rest: usize) -> (usize, i32, i32) {
        if let Some(state) = self.state.as_mut()
            && rest >= state.rest
            && rest <= state.end
        {
            // The new candidate starts inside the same token, past what the
            // last one saw: un-count the skipped gap rather than recounting
            // the suffix. An opener left behind stops opening, a closer left
            // behind stops closing.
            let from = state.rest;
            for ch in text[from..rest].chars() {
                match ch {
                    '(' => state.parens -= 1,
                    ')' => state.parens += 1,
                    '[' => state.brackets -= 1,
                    ']' => state.brackets += 1,
                    _ => {}
                }
            }
            state.rest = rest;
            return (state.end, state.parens, state.brackets);
        }
        let (end, parens, brackets) = scan_token_balances(text, rest);
        let (suffix_start, paren_stops, bracket_stops) = scan_trailing_suffix(text, end);
        self.state = Some(TokenState {
            end,
            rest,
            parens,
            brackets,
            suffix_start,
            paren_stops,
            bracket_stops,
            cut_tails: Vec::new(),
            cut_ready: false,
            cut_suffix: None,
        });
        (end, parens, brackets)
    }

    /// The candidate end after trailing cleanup, resolved from the cached
    /// suffix and the current balances rather than a fresh backward walk.
    ///
    /// Punctuation always trims, so the trim never stops inside it; each
    /// `)` trims only while `parens` is still negative — each one spent
    /// raising it back — and likewise for `]` and `brackets`. The trim then
    /// stops at the start of the suffix, or just past the first closer the
    /// balances cannot cover, whichever the walk would reach first.
    fn trim_end(&self, rest: usize, parens: i32, brackets: i32) -> usize {
        let state = self.state.as_ref().expect("balances ran first");
        let mut end = state.suffix_start;
        // The blocking closer is the one whose ordinal spends the budget:
        // the first `)` in scan order spends one, so with a budget of `-p`
        // the walk stops just past number `-p` — and with no budget left at
        // all it stops at the very first.
        let need_parens = if parens < 0 { (-parens) as usize } else { 0 };
        if let Some(stop) = state.paren_stops.get(need_parens) {
            end = end.max(*stop);
        }
        let need_brackets = if brackets < 0 {
            (-brackets) as usize
        } else {
            0
        };
        if let Some(stop) = state.bracket_stops.get(need_brackets) {
            end = end.max(*stop);
        }
        end.max(rest)
    }

    /// The candidate end after trailing cleanup short of a markup cut,
    /// resolved from a suffix record cached per cut rather than a fresh
    /// backward walk per candidate. The record depends only on the cut, so
    /// candidates sharing one reuse it; a new cut scans once and replaces
    /// it. Stop selection is identical to [`TokenScan::trim_end`].
    fn trim_cut_end(
        &mut self,
        text: &str,
        rest: usize,
        boundary: usize,
        parens: i32,
        brackets: i32,
    ) -> usize {
        let state = self.state.as_mut().expect("balances ran first");
        let hit = state
            .cut_suffix
            .as_ref()
            .is_some_and(|(at, _, _, _)| *at == boundary);
        if !hit {
            let record = scan_trailing_suffix(text, boundary);
            let state = self.state.as_mut().expect("balances ran first");
            state.cut_suffix = Some((boundary, record.0, record.1, record.2));
        }
        let state = self.state.as_ref().expect("balances ran first");
        let (_, suffix_start, paren_stops, bracket_stops) =
            state.cut_suffix.as_ref().expect("just stored");
        let mut end = *suffix_start;
        let need_parens = if parens < 0 { (-parens) as usize } else { 0 };
        if let Some(stop) = paren_stops.get(need_parens) {
            end = end.max(*stop);
        }
        let need_brackets = if brackets < 0 {
            (-brackets) as usize
        } else {
            0
        };
        if let Some(stop) = bracket_stops.get(need_brackets) {
            end = end.max(*stop);
        }
        end.max(rest)
    }

    /// The delimiter balances over `rest..cut`, where `cut` is a markup edge
    /// short of the token end.
    ///
    /// `parens` and `brackets` arrive counted over `rest..end`, so the cut
    /// needs them minus the tail over `cut..end`. The tail never depends on
    /// the candidate — only on the token — but the cut does: each candidate
    /// stops at the next edge after its own start, so repeated bold prefixes
    /// hand every candidate a different one. The first cut in a token
    /// therefore walks `cut..end` once, recording the tail for every edge it
    /// passes, and each later candidate answers from that record whatever
    /// edge it stops at. Walking the unchanged tail per candidate instead
    /// makes a formatted token of rejected prefixes joined to a long suffix
    /// quadratic in the peer's message.
    fn cut_balances(
        &mut self,
        text: &str,
        cut: usize,
        edges: &[usize],
        parens: i32,
        brackets: i32,
    ) -> (i32, i32) {
        let end = self.state.as_ref().expect("balances ran first").end;
        if !self.state.as_ref().expect("balances ran first").cut_ready {
            // One forward pass: the running balance over `cut..pos`, noted
            // at every edge, so each edge's tail is the total minus what
            // stood before it. In scan sign — `(` opens, `)` closes — which
            // is what the caller subtracts.
            let mut open = 0i32;
            let mut open_brackets = 0i32;
            let mut noted: Vec<(usize, i32, i32)> = Vec::new();
            let mut edge = edges.partition_point(|at| *at < cut);
            while edge < edges.len() && edges[edge] == cut {
                noted.push((cut, 0, 0));
                edge += 1;
            }
            for (offset, ch) in text[cut..end].char_indices() {
                match ch {
                    '(' => open += 1,
                    ')' => open -= 1,
                    '[' => open_brackets += 1,
                    ']' => open_brackets -= 1,
                    _ => {}
                }
                let pos = cut + offset + ch.len_utf8();
                while edge < edges.len() && edges[edge] <= pos && edges[edge] <= end {
                    noted.push((edges[edge], open, open_brackets));
                    edge += 1;
                }
                if edge < edges.len() && edges[edge] > end {
                    break;
                }
            }
            let state = self.state.as_mut().expect("balances ran first");
            state.cut_tails = noted
                .into_iter()
                .map(|(at, before, before_brackets)| {
                    (at, open - before, open_brackets - before_brackets)
                })
                .collect();
            state.cut_ready = true;
        }
        let state = self.state.as_ref().expect("balances ran first");
        match state.cut_tails.binary_search_by_key(&cut, |(at, _, _)| *at) {
            Ok(ix) => {
                let (_, tail, tail_brackets) = state.cut_tails[ix];
                (parens - tail, brackets - tail_brackets)
            }
            // A foreign edge table, which the callers never pass: every edge
            // here comes from `edges`, so the record holds it. Walked once
            // rather than cached, so the answer stays exact either way.
            Err(_) => {
                let mut tail = 0i32;
                let mut tail_brackets = 0i32;
                for ch in text[cut..end].chars() {
                    match ch {
                        '(' => tail += 1,
                        ')' => tail -= 1,
                        '[' => tail_brackets += 1,
                        ']' => tail_brackets -= 1,
                        _ => {}
                    }
                }
                (parens - tail, brackets - tail_brackets)
            }
        }
    }
}

/// If a link opens at `at`, its end after trailing cleanup, and whether it
/// is a bare `www.` needing a scheme. `None` is every other case: no prefix
/// here, a prefix in the middle of a word, or a prefix with nothing
/// link-shaped behind it.
///
/// `scan` carries the token end and the delimiter balances across calls.
/// They are only reused while the next candidate sits inside the same token,
/// so a token holding several prefixes still examines each of them —
/// jumping straight past the token would drop a valid link hiding behind an
/// invalid one.
///
/// `boundaries` are display offsets where a markup delimiter stood; see
/// [`find_links_with_boundaries`].
pub(crate) fn link_end_at(
    text: &str,
    at: usize,
    scan: &mut TokenScan,
    boundaries: &[usize],
) -> Option<(usize, bool)> {
    let bytes = text.as_bytes();
    let (prefix_len, bare) = prefix_at(bytes, at)?;
    if preceded_by_word_char(bytes, at, boundaries) {
        return None;
    }
    let rest = at + prefix_len;
    let (token, parens, brackets) = scan.balances(text, rest);
    // A markup boundary after the start ends the address: the display joins
    // the run to whatever follows it, but the source delimited them — in
    // `*https://example.com*x` the span ends before `x`, so the target stops
    // there too. A boundary at the start itself is preserved: the delimiter
    // stood before the address, which `preceded_by_word_char` already
    // honours.
    let boundary = match boundaries.partition_point(|edge| *edge <= at) {
        ix if ix < boundaries.len() => boundaries[ix],
        _ => usize::MAX,
    };
    let end = if boundary < token {
        // Only text that was ever marked up has boundaries, so the plain
        // walk is fine here: the cut balances come from the token's cached
        // tails rather than a fresh walk of the unchanged suffix per
        // candidate, then the trim takes what is left.
        if boundary <= rest {
            return None;
        }
        let (parens, brackets) = scan.cut_balances(text, boundary, boundaries, parens, brackets);
        scan.trim_cut_end(text, rest, boundary, parens, brackets)
    } else {
        scan.trim_end(rest, parens, brackets)
    };
    if end == rest || !host_is_plausible(&text[rest..end], bare) {
        return None;
    }
    Some((end, bare))
}

/// Where the whitespace-free token starting at `from` ends, and the
/// delimiter balances over it. An apostrophe does not end one: it sits
/// inside addresses like `https://example.com/O'Reilly`, and only a quote
/// left trailing at the very end is stripped later. A double quote does end
/// one, since no address ever contains a raw `"`.
fn scan_token_balances(text: &str, from: usize) -> (usize, i32, i32) {
    let mut end = from;
    let mut parens = 0i32;
    let mut brackets = 0i32;
    while let Some(ch) = text[end..].chars().next() {
        if ch.is_whitespace() || matches!(ch, '<' | '>' | '"') {
            break;
        }
        match ch {
            '(' => parens += 1,
            ')' => parens -= 1,
            '[' => brackets += 1,
            ']' => brackets -= 1,
            _ => {}
        }
        end += ch.len_utf8();
    }
    (end, parens, brackets)
}

/// The link prefix opening at `at`, if any, and whether it is a bare `www.`
/// that needs a scheme before it can be opened.
pub(crate) fn prefix_at(bytes: &[u8], at: usize) -> Option<(usize, bool)> {
    let rest = &bytes[at.min(bytes.len())..];
    if rest.len() >= 8 && rest[..8].eq_ignore_ascii_case(b"https://") {
        Some((8, false))
    } else if rest.len() >= 7 && rest[..7].eq_ignore_ascii_case(b"http://") {
        Some((7, false))
    } else if rest.len() >= 4 && rest[..4].eq_ignore_ascii_case(b"www.") {
        Some((4, true))
    } else {
        None
    }
}

/// Whether the prefix is really a link start rather than the middle of a
/// word: `abchttps://x` names no address, and neither does `foowww.bar`.
/// Only the byte before matters, and only ASCII letters and digits count — a
/// UTF-8 tail byte ending right here is neither, so no boundary check is
/// needed to read it. A markup boundary counts as a break: the delimiter
/// stood between the two, so the address never was mid-word.
fn preceded_by_word_char(bytes: &[u8], at: usize, boundaries: &[usize]) -> bool {
    if at == 0 || boundaries.binary_search(&at).is_ok() {
        return false;
    }
    bytes[at - 1].is_ascii_alphanumeric()
}

/// The token's trailing cleanup, walked once: where the run of trimmable
/// characters starts, and — in scan order, from the token end backward —
/// the end each `)` or `]` would leave behind if the balances ran out at
/// it. A candidate's trim is then one lookup per delimiter kind against the
/// current balances rather than a fresh walk of the closers.
fn scan_trailing_suffix(text: &str, end: usize) -> (usize, Vec<usize>, Vec<usize>) {
    let mut start = end;
    let mut paren_stops = Vec::new();
    let mut bracket_stops = Vec::new();
    while start > 0 {
        let Some(ch) = text[..start].chars().next_back() else {
            break;
        };
        if trailing_punctuation(ch) {
            start -= ch.len_utf8();
        } else if ch == ')' {
            // A walk blocked here would leave this closer in, so the end it
            // leaves behind is just past it.
            paren_stops.push(start);
            start -= 1;
        } else if ch == ']' {
            bracket_stops.push(start);
            start -= 1;
        } else {
            break;
        }
    }
    (start, paren_stops, bracket_stops)
}

/// Sentence punctuation that never belongs to the address — the trailing
/// cleanup cuts back off it.
fn trailing_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '.' | ','
            | ';'
            | ':'
            | '!'
            | '?'
            | '。'
            | '．'
            | '，'
            | '、'
            | '！'
            | '？'
            | '；'
            | '：'
            | '…'
            | '\''
            | '’'
            | '”'
            | '»'
            | '›'
            | '」'
            | '』'
    )
}

/// Whether the part after the prefix could be a host: non-empty, and dotted
/// for a bare `www.` — `www.` with nothing dotted after it is somebody
/// trailing off, not an address. An explicit scheme is the writer saying
/// "this is a link", so `http://localhost:8080/x` passes on non-emptiness
/// alone.
fn host_is_plausible(rest: &str, bare: bool) -> bool {
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() {
        return false;
    }
    if bare {
        host.contains('.') && !host.starts_with('.') && !host.ends_with('.')
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_text_has_no_links() {
        assert!(find_links("hello there").is_empty());
        assert!(find_links("2 * 3 * 4").is_empty());
    }

    #[test]
    fn an_https_address_is_a_link() {
        let links = find_links("see https://example.com/docs now");
        assert_eq!(links.len(), 1);
        assert_eq!(
            &"see https://example.com/docs now"[links[0].range.clone()],
            "https://example.com/docs"
        );
        assert_eq!(links[0].target, "https://example.com/docs");
    }

    #[test]
    fn a_bare_www_address_gains_a_scheme() {
        let links = find_links("try www.example.com/a?b=1#c");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "https://www.example.com/a?b=1#c");
    }

    #[test]
    fn sentence_punctuation_belongs_to_the_sentence() {
        for (source, address) in [
            ("see https://example.com.", "https://example.com"),
            ("see https://example.com, ok?", "https://example.com"),
            ("wow! https://example.com!", "https://example.com"),
            ("(see https://example.com)", "https://example.com"),
        ] {
            let links = find_links(source);
            assert_eq!(links.len(), 1, "for {source:?}");
            assert_eq!(&source[links[0].range.clone()], address, "for {source:?}");
            assert_eq!(links[0].target, address, "for {source:?}");
        }
    }

    #[test]
    fn balanced_parens_belong_to_the_address() {
        let source = "https://en.wikipedia.org/wiki/Rust_(language) is long";
        let links = find_links(source);
        assert_eq!(links.len(), 1);
        assert_eq!(
            &source[links[0].range.clone()],
            "https://en.wikipedia.org/wiki/Rust_(language)"
        );
    }

    #[test]
    fn several_links_are_all_found() {
        let links = find_links("https://one.example and http://two.example/x");
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].target, "https://one.example");
        assert_eq!(links[1].target, "http://two.example/x");
    }

    #[test]
    fn a_prefix_inside_a_word_is_not_a_link() {
        assert!(find_links("foowww.example.com").is_empty());
        assert!(find_links("xhttps://example.com").is_empty());
    }

    #[test]
    fn an_empty_or_hostless_address_is_not_a_link() {
        assert!(find_links("http:// next").is_empty());
        assert!(find_links("see http://").is_empty());
        assert!(find_links("www. is not an address").is_empty());
        assert!(find_links("www. trailing").is_empty());
    }

    #[test]
    fn scheme_case_does_not_matter() {
        let links = find_links("HTTPS://EXAMPLE.COM/x");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "HTTPS://EXAMPLE.COM/x");
    }

    #[test]
    fn ranges_cover_exactly_the_address() {
        let source = "a https://example.com b";
        let links = find_links(source);
        assert_eq!(links.len(), 1);
        assert_eq!(&source[links[0].range.clone()], "https://example.com");
        assert!(source.is_char_boundary(links[0].range.start));
        assert!(source.is_char_boundary(links[0].range.end));
    }

    #[test]
    fn unicode_around_a_link_is_left_alone() {
        let source = "🎉 https://example.com/x 🎉";
        let links = find_links(source);
        assert_eq!(links.len(), 1);
        assert_eq!(&source[links[0].range.clone()], "https://example.com/x");
    }

    #[test]
    fn an_apostrophe_inside_a_link_is_kept() {
        let source = "see https://example.com/O'Reilly now";
        let links = find_links(source);
        assert_eq!(links.len(), 1);
        assert_eq!(
            &source[links[0].range.clone()],
            "https://example.com/O'Reilly"
        );
        assert_eq!(links[0].target, "https://example.com/O'Reilly");
    }

    #[test]
    fn surrounding_quotes_are_not_part_of_the_link() {
        for (source, address) in [
            ("'https://example.com/x'", "https://example.com/x"),
            (
                "see 'https://example.com/O'Reilly'",
                "https://example.com/O'Reilly",
            ),
            ("“https://example.com/x”", "https://example.com/x"),
            ("‘https://example.com/x’", "https://example.com/x"),
            ("«https://example.com/x»", "https://example.com/x"),
        ] {
            let links = find_links(source);
            assert_eq!(links.len(), 1, "for {source:?}");
            assert_eq!(&source[links[0].range.clone()], address, "for {source:?}");
            assert_eq!(links[0].target, address, "for {source:?}");
        }
    }

    #[test]
    fn cjk_sentence_punctuation_belongs_to_the_sentence() {
        for (source, address) in [
            ("see https://example.com/path。", "https://example.com/path"),
            ("see https://example.com/path，", "https://example.com/path"),
            ("see https://example.com/path、", "https://example.com/path"),
            ("see https://example.com/path！", "https://example.com/path"),
            ("see https://example.com/path？", "https://example.com/path"),
            ("see https://example.com/path；", "https://example.com/path"),
            ("see https://example.com/path：", "https://example.com/path"),
            ("see https://example.com/path．", "https://example.com/path"),
            ("see https://example.com/path…", "https://example.com/path"),
        ] {
            let links = find_links(source);
            assert_eq!(links.len(), 1, "for {source:?}");
            assert_eq!(&source[links[0].range.clone()], address, "for {source:?}");
            assert_eq!(links[0].target, address, "for {source:?}");
        }
    }

    /// A token can hold many prefixes and still one valid link at its end.
    /// The rejection path reuses the token end rather than jumping past the
    /// token, so the valid link behind the invalid prefixes is still found.
    #[test]
    fn a_valid_link_behind_rejected_prefixes_is_still_found() {
        let source = format!("{}https://example.com/ok", "http:///".repeat(200));
        let links = find_links(&source);
        assert_eq!(links.len(), 1);
        assert_eq!(&source[links[0].range.clone()], "https://example.com/ok");
        assert_eq!(links[0].target, "https://example.com/ok");
    }

    /// One whitespace-free token of repeated invalid prefixes: each
    /// rejection reuses the token end, so this is one forward scan plus one
    /// cheap check per prefix. Rescanning the token per prefix would make
    /// this quadratic in the peer's message.
    #[test]
    fn many_rejected_prefixes_in_one_token_stay_fast() {
        let source = "http:///".repeat(8000);
        let started = wacore::time::Instant::now();
        let links = find_links(&source);
        let elapsed = started.elapsed();
        assert!(links.is_empty());
        assert!(
            elapsed.as_secs() < 10,
            "took {elapsed:?} for {} bytes",
            source.len()
        );
    }

    /// An address followed by a long run of unmatched closers: the balance
    /// is counted once, so trimming is linear rather than quadratic in the
    /// peer's message.
    #[test]
    fn many_unmatched_closers_trim_fast() {
        let closers = ")".repeat(20_000);
        let source = format!("https://example.com/x{closers}");
        let started = wacore::time::Instant::now();
        let links = find_links(&source);
        let elapsed = started.elapsed();
        assert_eq!(links.len(), 1);
        assert_eq!(&source[links[0].range.clone()], "https://example.com/x");
        assert_eq!(links[0].target, "https://example.com/x");
        assert!(
            elapsed.as_secs() < 10,
            "took {elapsed:?} for {} bytes",
            source.len()
        );

        // The balanced half stays: only the sentence's closers go.
        let source = format!("https://en.wikipedia.org/wiki/Rust_(language){closers}");
        let links = find_links(&source);
        assert_eq!(links.len(), 1);
        assert_eq!(
            &source[links[0].range.clone()],
            "https://en.wikipedia.org/wiki/Rust_(language)"
        );
    }

    /// One whitespace-free token of repeated invalid prefixes ending in a
    /// real address: each rejection reuses the token's delimiter balances
    /// rather than recounting the remaining suffix, or this is quadratic in
    /// the peer's message while still owing the trailing link its range.
    #[test]
    fn many_rejected_prefixes_before_a_valid_link_stay_fast() {
        let source = format!("{}https://example.com/ok", "http:///".repeat(20_000));
        let started = wacore::time::Instant::now();
        let links = find_links(&source);
        let elapsed = started.elapsed();
        assert_eq!(links.len(), 1);
        assert_eq!(&source[links[0].range.clone()], "https://example.com/ok");
        assert_eq!(links[0].target, "https://example.com/ok");
        assert!(
            elapsed.as_secs() < 10,
            "took {elapsed:?} for {} bytes",
            source.len()
        );
    }

    /// A delimiter the markup removed still separates: `*label*https://...`
    /// displays joined, but the address was delimited in the source, so the
    /// span edge counts as a boundary and the link is found.
    #[test]
    fn a_link_after_a_removed_marker_is_still_a_link() {
        let rich = crate::rich_text::parse("*label*https://example.com");
        assert_eq!(rich.text, "labelhttps://example.com");
        let links = find_links_in(&rich);
        assert_eq!(links.len(), 1);
        assert_eq!(&rich.text[links[0].range.clone()], "https://example.com");
        assert_eq!(links[0].target, "https://example.com");
    }

    /// A boundary after the start ends the address: `*https://example.com*x`
    /// parses to `https://example.comx` with the span ending before `x`, so
    /// the link stops at the boundary instead of swallowing the suffix into
    /// the target. A boundary at the start itself still counts as a break.
    #[test]
    fn a_formatting_boundary_after_the_start_ends_the_link() {
        let text = "https://example.comx";
        let links = find_links_with_boundaries(text, &[0, "https://example.com".len()]);
        assert_eq!(links.len(), 1);
        assert_eq!(&text[links[0].range.clone()], "https://example.com");
        assert_eq!(links[0].target, "https://example.com");
    }

    /// Rejected prefixes ahead of a long run of unmatched closers: every
    /// candidate shares the token, so the suffix is walked once and each
    /// rejection reuses it — otherwise this is quadratic in the peer's
    /// message while finding nothing at all.
    #[test]
    fn many_rejected_prefixes_before_unmatched_closers_stay_fast() {
        let source = format!("{}{}", "http:///".repeat(20_000), ")".repeat(60_000));
        let started = wacore::time::Instant::now();
        let links = find_links(&source);
        let elapsed = started.elapsed();
        assert!(links.is_empty());
        assert!(
            elapsed.as_secs() < 10,
            "took {elapsed:?} for {} bytes",
            source.len()
        );
    }

    /// A formatted token of rejected prefixes joined to a long unformatted
    /// suffix: every candidate stops at the same markup boundary, so the
    /// boundary-adjusted balances come from one walk of the unchanged tail —
    /// otherwise each rejection rescans the whole suffix and this is
    /// quadratic in the peer's message while finding nothing at all.
    #[test]
    fn many_rejected_prefixes_before_a_boundary_before_a_long_suffix_stay_fast() {
        let repeats = 20_000;
        let source = format!(
            "*{}*{}",
            "http:///".repeat(repeats),
            "a".repeat(8 * repeats)
        );
        let started = wacore::time::Instant::now();
        let rich = crate::rich_text::parse(&source);
        let links = find_links_in(&rich);
        let elapsed = started.elapsed();
        assert!(links.is_empty());
        assert!(
            elapsed.as_secs() < 10,
            "took {elapsed:?} for {} bytes",
            source.len()
        );
    }

    /// A cut short of the token end trims the same way the token end does:
    /// unmatched closers before the boundary go, balanced ones stay.
    #[test]
    fn a_cut_before_closers_trims_to_the_address() {
        let text = "https://example.com/x)))tail";
        let cut = "https://example.com/x)))".len();
        let links = find_links_with_boundaries(text, &[0, cut]);
        assert_eq!(links.len(), 1);
        assert_eq!(&text[links[0].range.clone()], "https://example.com/x");
        assert_eq!(links[0].target, "https://example.com/x");

        let text = "https://en.wikipedia.org/wiki/Rust_(language))))tail";
        let cut = "https://en.wikipedia.org/wiki/Rust_(language))))".len();
        let links = find_links_with_boundaries(text, &[0, cut]);
        assert_eq!(links.len(), 1);
        assert_eq!(
            &text[links[0].range.clone()],
            "https://en.wikipedia.org/wiki/Rust_(language)"
        );
    }

    /// Rejected prefixes ahead of a long run of unmatched closers ahead of a
    /// markup boundary: every candidate cuts at the same offset, so the cut
    /// suffix is walked once and each rejection reuses it — otherwise every
    /// candidate trims all the closers and this is quadratic in the peer's
    /// message while finding nothing at all.
    #[test]
    fn many_rejected_prefixes_before_closers_before_a_boundary_stay_fast() {
        let repeats = 20_000;
        let source = format!("*{}{}*tail", "http:///".repeat(repeats), ")".repeat(60_000));
        let started = wacore::time::Instant::now();
        let rich = crate::rich_text::parse(&source);
        let links = find_links_in(&rich);
        let elapsed = started.elapsed();
        assert!(links.is_empty());
        assert!(
            elapsed.as_secs() < 10,
            "took {elapsed:?} for {} bytes",
            source.len()
        );
    }
}
