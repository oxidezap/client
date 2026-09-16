//! Account profile identities: the one rule every process agrees on.
//!
//! One account is one daemon over one store — socket, lock, media cache and
//! database all derive from the profile id — so an id is validated, never
//! sanitized. Sanitizing maps distinct inputs onto one profile (`wo/rk` and
//! `work` must never converge); rejecting keeps them apart. The charset is
//! also what makes the `accounts use` export line safe to `eval`: after
//! validation no quote, space, `$` or backtick can be in the value.
//!
//! The rule: `[A-Za-z0-9][A-Za-z0-9_-]*`. Nonempty, ASCII only, leading
//! alphanumeric so an id can never parse as a flag or a hidden/dot path.
//! `"default"` passes the charset on purpose: it names the default profile
//! and each caller maps it to "unset" itself.

/// The account id in `raw`, or `None` when it is not a valid profile name.
///
/// Pure and total: no environment, no I/O, no platform split, so every
/// caller — CLI flag, `accounts use`, daemon flag, `OXIDEZAP_ACCOUNT` — can
/// share exactly this check.
#[must_use]
pub fn validate_account_id(raw: &str) -> Option<String> {
    let mut chars = raw.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphanumeric() => (),
        _ => return None,
    }
    if raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Some(raw.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_ids_pass_through_unchanged() {
        for id in ["work", "a", "A1", "w-r_k", "Zal"] {
            assert_eq!(validate_account_id(id), Some(id.to_string()), "{id}");
        }
        assert_eq!(validate_account_id("default"), Some("default".to_string()));
    }

    #[test]
    fn invalid_ids_are_rejected_not_reshaped() {
        // The sanitizer this replaced would have mapped several of these
        // onto real profiles (`wo/rk` → `work`); rejection keeps them apart.
        for id in [
            "",
            "-work",
            "_work",
            ".work",
            "wo/rk",
            "wo!rk",
            "wo rk",
            "work$HOME",
            "work`id`",
            "work'x",
            "wörk",
            "a/b",
            "../x",
        ] {
            assert_eq!(validate_account_id(id), None, "{id}");
        }
    }
}
