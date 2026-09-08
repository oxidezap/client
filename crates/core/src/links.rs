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
    let bytes = text.as_bytes();
    let mut links = Vec::new();
    let mut at = 0;
    // Where the whitespace-free token around the last examined candidate
    // ends. A rejected candidate stays inside its token, so the next
    // candidate in the same token reuses this instead of scanning the token
    // again — without it one long token of repeated prefixes scans once per
    // prefix, which is quadratic in the peer's message.
    let mut token_end: Option<usize> = None;
    while at < bytes.len() {
        let Some((end, bare)) = link_end_at(text, at, &mut token_end) else {
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

/// If a link opens at `at`, its end after trailing cleanup, and whether it
/// is a bare `www.` needing a scheme. `None` is every other case: no prefix
/// here, a prefix in the middle of a word, or a prefix with nothing
/// link-shaped behind it.
///
/// `token_end` caches the token end across calls. It is only reused while
/// the next candidate sits inside the same token, so a token holding several
/// prefixes still examines each of them — jumping straight past the token
/// would drop a valid link hiding behind an invalid one.
pub(crate) fn link_end_at(
    text: &str,
    at: usize,
    token_end: &mut Option<usize>,
) -> Option<(usize, bool)> {
    let bytes = text.as_bytes();
    let (prefix_len, bare) = prefix_at(bytes, at)?;
    if preceded_by_word_char(bytes, at) {
        return None;
    }
    let rest = at + prefix_len;
    let token = match *token_end {
        Some(cached) if rest <= cached => cached,
        _ => {
            let scanned = scan_token_end(text, rest);
            *token_end = Some(scanned);
            scanned
        }
    };
    let end = trim_trailing_punctuation(text, rest, token);
    if end == rest || !host_is_plausible(&text[rest..end], bare) {
        return None;
    }
    Some((end, bare))
}

/// Where the whitespace-free token starting at `from` ends. An apostrophe
/// does not end one: it sits inside addresses like
/// `https://example.com/O'Reilly`, and only a quote left trailing at the
/// very end is stripped later. A double quote does end one, since no address
/// ever contains a raw `"`.
fn scan_token_end(text: &str, from: usize) -> usize {
    let mut end = from;
    while let Some(ch) = text[end..].chars().next() {
        if ch.is_whitespace() || matches!(ch, '<' | '>' | '"') {
            break;
        }
        end += ch.len_utf8();
    }
    end
}

/// The link prefix opening at `at`, if any, and whether it is a bare `www.`
/// that needs a scheme before it can be opened.
fn prefix_at(bytes: &[u8], at: usize) -> Option<(usize, bool)> {
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
/// needed to read it.
fn preceded_by_word_char(bytes: &[u8], at: usize) -> bool {
    at > 0 && bytes[at - 1].is_ascii_alphanumeric()
}

/// Cut the sentence back off the address: trailing `.` `,` `;` `:` `!` `?`
/// (and the fullwidth and ideographic equivalents CJK keyboards produce),
/// a trailing quote that closed a quoted address but never belonged to it,
/// and a `)` or `]` that closes nothing in the candidate.
///
/// Only the trailing quote goes: an apostrophe inside the address, as in
/// `https://example.com/O'Reilly`, is kept. A raw `"` never reaches this
/// far, since it already ends the token.
fn trim_trailing_punctuation(text: &str, rest: usize, mut end: usize) -> usize {
    // Both balances up front, so trimming a run of unmatched closers is one
    // scan plus one step per closer. Calling `closers_outnumber_openers` per
    // removed character rescanned the whole shrinking candidate each time,
    // which is quadratic in a peer-controlled message of `))))…`.
    let mut parens = 0i32;
    let mut brackets = 0i32;
    for ch in text[rest..end].chars() {
        match ch {
            '(' => parens += 1,
            ')' => parens -= 1,
            '[' => brackets += 1,
            ']' => brackets -= 1,
            _ => {}
        }
    }
    while end > rest {
        let Some(ch) = text[..end].chars().next_back() else {
            break;
        };
        if matches!(
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
        ) {
            end -= ch.len_utf8();
        } else if ch == ')' {
            // Only a closer leaves; an opener never sits at the trailing
            // edge trimmed here, so the balance only ever rises back.
            if parens < 0 {
                end -= 1;
                parens += 1;
            } else {
                break;
            }
        } else if ch == ']' {
            if brackets < 0 {
                end -= 1;
                brackets += 1;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    end
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
}
