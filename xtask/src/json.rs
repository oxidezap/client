//! Just enough JSON to read three fields out of a GitHub API response.
//!
//! `jq` is what the shell used, and pulling a serde graph in to replace it
//! would cost this directory the property that makes it buildable on its own.
//! What is actually asked of it is `.state`, `.head.sha` and `.sha`: string
//! values at known paths. So this is a scanner rather than a document model —
//! it walks the text, skipping whatever it is not looking at, and answers the
//! one path it was given.
//!
//! It is a real parse rather than a search for a substring, and that matters:
//! a pull request whose *title* is `"state": "open"` would otherwise decide
//! whether its own preview gets published.

use std::str::CharIndices;

/// Read the string at a dotted path — `head.sha` — out of a JSON object.
///
/// Answers `None` for a path that is absent or whose value is not a string,
/// which the callers read as "the API did not say", and that is an answer they
/// refuse to guess from rather than a default.
pub fn string_at(text: &str, path: &str) -> Option<String> {
    let mut p = Parser::new(text);
    p.skip_ws();
    p.value_at(&path.split('.').collect::<Vec<_>>())
}

struct Parser<'a> {
    chars: CharIndices<'a>,
    peeked: Option<(usize, char)>,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Parser {
            chars: text.char_indices(),
            peeked: None,
        }
    }

    fn next(&mut self) -> Option<(usize, char)> {
        match self.peeked.take() {
            Some(c) => Some(c),
            None => self.chars.next(),
        }
    }

    fn peek(&mut self) -> Option<(usize, char)> {
        if self.peeked.is_none() {
            self.peeked = self.chars.next();
        }
        self.peeked
    }

    fn skip_ws(&mut self) {
        while let Some((_, c)) = self.peek() {
            if c.is_ascii_whitespace() {
                self.next();
            } else {
                break;
            }
        }
    }

    fn eat(&mut self, want: char) -> bool {
        self.skip_ws();
        if let Some((_, c)) = self.peek()
            && c == want
        {
            self.next();
            return true;
        }
        false
    }

    /// Walk `path` from the value about to be read, answering the string it
    /// arrives at.
    fn value_at(&mut self, path: &[&str]) -> Option<String> {
        let Some(&key) = path.first() else {
            // Arrived: the value here is the answer, and only if it is a
            // string.
            return self.string();
        };
        self.skip_ws();
        if !self.eat('{') {
            // The path continues into something that is not an object.
            self.skip_value();
            return None;
        }
        loop {
            self.skip_ws();
            if self.eat('}') {
                return None;
            }
            let name = self.string()?;
            self.skip_ws();
            if !self.eat(':') {
                return None;
            }
            if name == key {
                return self.value_at(&path[1..]);
            }
            self.skip_value();
            self.skip_ws();
            if self.eat(',') {
                continue;
            }
            // `}` or malformed; either way this object has no such key.
            return None;
        }
    }

    /// Read a string literal, resolving the escapes JSON defines.
    fn string(&mut self) -> Option<String> {
        self.skip_ws();
        if !self.eat('"') {
            return None;
        }
        let mut out = String::new();
        loop {
            let (_, c) = self.next()?;
            match c {
                '"' => return Some(out),
                '\\' => {
                    let (_, esc) = self.next()?;
                    match esc {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'u' => {
                            let hi = self.hex4()?;
                            // A surrogate pair is two escapes; anything else
                            // is one scalar.
                            let ch = if (0xd800..0xdc00).contains(&hi) {
                                let (_, slash) = self.next()?;
                                let (_, u) = self.next()?;
                                if slash != '\\' || u != 'u' {
                                    return None;
                                }
                                let lo = self.hex4()?;
                                if !(0xdc00..0xe000).contains(&lo) {
                                    return None;
                                }
                                let combined = 0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00);
                                char::from_u32(combined)?
                            } else {
                                char::from_u32(hi)?
                            };
                            out.push(ch);
                        }
                        _ => return None,
                    }
                }
                c => out.push(c),
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let mut n = 0u32;
        for _ in 0..4 {
            let (_, c) = self.next()?;
            n = n * 16 + c.to_digit(16)?;
        }
        Some(n)
    }

    /// Step over the value about to be read, whatever it is.
    fn skip_value(&mut self) {
        self.skip_ws();
        match self.peek() {
            Some((_, '"')) => {
                self.string();
            }
            Some((_, '{')) => self.skip_nested('{', '}'),
            Some((_, '[')) => self.skip_nested('[', ']'),
            _ => {
                // A number, `true`, `false` or `null`: everything up to the
                // next structural character.
                while let Some((_, c)) = self.peek() {
                    if c == ',' || c == '}' || c == ']' || c.is_ascii_whitespace() {
                        break;
                    }
                    self.next();
                }
            }
        }
    }

    /// Step over a balanced bracketed run, counting depth and treating a
    /// string as opaque — a `}` inside one closes nothing.
    fn skip_nested(&mut self, open: char, close: char) {
        if !self.eat(open) {
            return;
        }
        let mut depth = 1usize;
        while depth > 0 {
            self.skip_ws();
            match self.peek() {
                None => return,
                Some((_, '"')) => {
                    if self.string().is_none() {
                        return;
                    }
                }
                Some((_, c)) if c == open => {
                    self.next();
                    depth += 1;
                }
                Some((_, c)) if c == close => {
                    self.next();
                    depth -= 1;
                }
                Some(_) => {
                    self.next();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::string_at;

    #[test]
    fn a_top_level_string_is_read() {
        assert_eq!(string_at(r#"{"sha":"abc"}"#, "sha").as_deref(), Some("abc"));
    }

    #[test]
    fn a_nested_string_is_read() {
        let text = r#"{"number":12,"head":{"ref":"topic","sha":"deadbeef"},"state":"open"}"#;
        assert_eq!(string_at(text, "head.sha").as_deref(), Some("deadbeef"));
        assert_eq!(string_at(text, "state").as_deref(), Some("open"));
    }

    /// The reason this is a parser and not a `grep`. A pull request whose
    /// title says `"state": "open"` must not answer for the pull request.
    #[test]
    fn a_value_that_looks_like_a_field_is_not_one() {
        let text = r#"{"title":"fix \"state\": \"open\" handling","state":"closed"}"#;
        assert_eq!(string_at(text, "state").as_deref(), Some("closed"));
    }

    /// The API answers with far more than the three fields anybody reads, and
    /// the objects and arrays in between have to be stepped over rather than
    /// descended into.
    #[test]
    fn keys_of_skipped_objects_are_not_matched() {
        let text = r#"{"user":{"state":"ghost"},"labels":[{"state":"stale"}],"state":"open"}"#;
        assert_eq!(string_at(text, "state").as_deref(), Some("open"));
    }

    #[test]
    fn escapes_and_unicode_are_resolved() {
        let text = r#"{"a":"line\nbreak é 😀"}"#;
        assert_eq!(string_at(text, "a").as_deref(), Some("line\nbreak é 😀"));
    }

    #[test]
    fn an_absent_or_non_string_path_is_no_answer() {
        let text = r#"{"state":"open","number":12,"merged":false,"head":null}"#;
        assert_eq!(string_at(text, "missing"), None);
        assert_eq!(string_at(text, "number"), None);
        assert_eq!(string_at(text, "merged"), None);
        assert_eq!(string_at(text, "head.sha"), None);
    }

    #[test]
    fn truncated_input_answers_nothing_rather_than_panicking() {
        assert_eq!(string_at(r#"{"state":"op"#, "state"), None);
        assert_eq!(string_at(r#"{"head":{"sha":"#, "head.sha"), None);
        assert_eq!(string_at("", "state"), None);
        assert_eq!(string_at("[1,2,3]", "state"), None);
    }
}
