//! Inline `@`-mentions in message text.
//!
//! The wire carries a mention as `@` plus the address's user part, beside the
//! full JID in the message's mention list. A phone draws the contact's name
//! there; the raw digits are an internal address, meaningless for a LID and
//! noise for a phone number. This is the mechanical half of that: replace the
//! tokens, given the names. Who a JID resolves to — address book, push name,
//! or the number — is the session's answer, which is why this takes names
//! rather than JIDs.

/// Replace `@<user>` tokens in `text` with the display name for that user.
///
/// `mentions` maps a JID user part to the name to draw, e.g. `("559900000002",
/// "Ana")`. A token whose digits name nobody in the map is left as typed: it
/// is either not a mention at all — a price somebody wrote out — or a mention
/// whose JID never arrived, and digits the sender typed are honest while a
/// name guessed from them would not be.
///
/// The scan is one pass over the bytes. Names are pushed literally and never
/// re-scanned, so a name carrying `@` or digits cannot become a second
/// mention.
///
/// A name carrying WhatsApp's markup markers is another matter: the GUI runs
/// the whole content through `parse_rich_text`, so a contact called `_Ana_`
/// would come out italic and could even pair with markers the sender typed
/// around the mention. The markers in a substituted name are redrawn as their
/// fullwidth lookalikes, which no marker scan treats as delimiters — the name
/// stays literal while the sender's own markup around it still parses.
pub fn format_mentions(text: &str, mentions: &[(&str, &str)]) -> String {
    if !text.contains('@') || mentions.is_empty() {
        return text.to_string();
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut rest = 0;
    while rest < bytes.len() {
        let Some(at) = text[rest..].find('@') else {
            out.push_str(&text[rest..]);
            break;
        };
        let token = rest + at;
        let mut digits = token + 1;
        while digits < bytes.len() && bytes[digits].is_ascii_digit() {
            digits += 1;
        }
        if digits == token + 1 {
            out.push_str(&text[rest..digits]);
            rest = digits;
            continue;
        }
        let user = &text[token + 1..digits];
        match mentions
            .iter()
            .find(|(known, _)| *known == user)
            .map(|(_, name)| *name)
            .filter(|name| !name.is_empty())
        {
            Some(name) => {
                out.push_str(&text[rest..token]);
                out.push('@');
                out.push_str(&literal_name(name));
            }
            None => out.push_str(&text[rest..digits]),
        }
        rest = digits;
    }
    out
}

/// A substituted name with its markup markers neutralized.
///
/// Only the four ASCII delimiters `parse_rich_text` acts on are redrawn; a
/// name without them borrows rather than allocates, which is nearly every
/// name drawn.
fn literal_name(name: &str) -> std::borrow::Cow<'_, str> {
    if !name.bytes().any(|b| matches!(b, b'*' | b'_' | b'~' | b'`')) {
        return std::borrow::Cow::Borrowed(name);
    }
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '*' => out.push('＊'),
            '_' => out.push('＿'),
            '~' => out.push('～'),
            '`' => out.push('｀'),
            _ => out.push(ch),
        }
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mentions<'a>(pairs: &'a [(&'a str, &'a str)]) -> &'a [(&'a str, &'a str)] {
        pairs
    }

    #[test]
    fn text_without_mentions_is_returned_unchanged() {
        let text = "bom dia, como vai?";
        assert_eq!(format_mentions(text, mentions(&[("1", "Ana")])), text);
    }

    #[test]
    fn a_mention_draws_the_name_not_the_digits() {
        assert_eq!(
            format_mentions(
                "oi @559900000002, tudo bem?",
                mentions(&[("559900000002", "Ana")])
            ),
            "oi @Ana, tudo bem?"
        );
    }

    #[test]
    fn several_mentions_are_all_renamed() {
        assert_eq!(
            format_mentions(
                "@559900000002 e @559900000003 venham aqui",
                mentions(&[("559900000002", "Ana"), ("559900000003", "Beto")])
            ),
            "@Ana e @Beto venham aqui"
        );
    }

    #[test]
    fn an_unknown_token_is_left_as_typed() {
        // A price somebody wrote out is not a mention, and a mention whose
        // JID never arrived is not a name to guess at.
        let text = "custa @500 reais para @559900000002";
        assert_eq!(
            format_mentions(text, mentions(&[("559900000002", "Ana")])),
            "custa @500 reais para @Ana"
        );
    }

    #[test]
    fn a_bare_at_sign_is_not_a_mention() {
        let text = "me chama @ amanha @!";
        assert_eq!(format_mentions(text, mentions(&[("1", "Ana")])), text);
    }

    #[test]
    fn an_empty_name_leaves_the_token() {
        let text = "oi @559900000002";
        assert_eq!(
            format_mentions(text, mentions(&[("559900000002", "")])),
            text
        );
    }

    #[test]
    fn a_name_is_pushed_literally_never_rescanned() {
        // Were the name re-scanned, `@Ana 123` would match user `123`.
        assert_eq!(
            format_mentions("oi @1", mentions(&[("1", "@Ana 123"), ("123", "Beto")])),
            "oi @@Ana 123"
        );
    }

    #[test]
    fn a_mention_at_either_end_of_the_text() {
        assert_eq!(
            format_mentions(
                "@559900000002 bom dia",
                mentions(&[("559900000002", "Ana")])
            ),
            "@Ana bom dia"
        );
        assert_eq!(
            format_mentions(
                "bom dia @559900000002",
                mentions(&[("559900000002", "Ana")])
            ),
            "bom dia @Ana"
        );
    }

    #[test]
    fn a_name_carrying_markup_stays_literal() {
        // The bubble is drawn through `parse_rich_text`, so a contact called
        // `_Ana_` would otherwise come out italic — and could pair with
        // markers the sender typed around the mention.
        let drawn = format_mentions("oi @559900000002!", mentions(&[("559900000002", "_Ana_")]));
        assert_eq!(drawn, "oi @＿Ana＿!");
        let rich = crate::parse_rich_text(&drawn);
        assert!(rich.is_plain(), "{rich:?} must carry no emphasis");
        assert_eq!(rich.text, "oi @＿Ana＿!");
    }

    #[test]
    fn markup_around_a_mention_still_parses() {
        // Only the substituted name is neutralized; the sender's own markup
        // keeps working.
        let drawn = format_mentions("*oi* @559900000002", mentions(&[("559900000002", "_Ana_")]));
        let rich = crate::parse_rich_text(&drawn);
        assert_eq!(rich.text, "oi @＿Ana＿");
        assert!(!rich.is_plain(), "{rich:?} must keep the sender's bold");
    }
}
