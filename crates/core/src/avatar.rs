//! Avatar identity, spelled once so both halves agree.
//!
//! A profile picture is addressed two ways: by WhatsApp's own picture id, which
//! is what a metadata refresh compares against, and by the media cache key,
//! which is what the bytes on disk are named. The key is a pure function of the
//! address and the picture id, so it belongs here rather than beside either
//! user — a session that derived it one way and a daemon that derived it
//! another would be two entries for one picture.

/// A field of a cache key, spelled in the characters a cache file name accepts.
///
/// Escapes rather than folds: a byte outside `[A-Za-z0-9_-]` becomes `.` and
/// its two hex digits, and `.` escapes itself, so the spelling is reversible
/// and two different addresses cannot meet under one key. A fold is what made
/// `a/b` and `a?b` one entry.
fn safe_component(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
            result.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(result, ".{byte:02X}");
        }
    }
    result
}

/// The media cache key holding the picture `picture_id` for `jid`.
///
/// Deterministic, so a restarted process can address bytes it never fetched
/// this run: the durable descriptor stores this key beside the picture id, and
/// a cache entry written under it before the restart is a hit rather than a
/// second download. The spelling is the one the daemon wrote before this moved
/// here, so an entry already on disk keeps its name.
pub fn cache_key(jid: &str, picture_id: &str) -> String {
    let jid = safe_component(jid);
    let id = safe_component(picture_id);
    format!("a-{}-{jid}-{}-{id}", jid.len(), id.len())
}

#[cfg(test)]
mod tests {
    use super::cache_key;

    #[test]
    fn keys_are_safe_and_distinct() {
        assert_eq!(cache_key("jid-1", "picture-1"), "a-5-jid-1-9-picture-1");
        assert_ne!(
            cache_key("jid-1", "picture-1"),
            cache_key("jid-2", "picture-1")
        );
        assert_ne!(cache_key("a/b", "picture"), cache_key("a?b", "picture"));
        assert!(!cache_key("../../avatar", "picture").contains('/'));
    }

    /// The spelling is load-bearing: a cache entry written under it before this
    /// helper moved out of the daemon has to keep its name, or the restart the
    /// key exists for orphans every cached avatar once. `@` and `.` in an
    /// address both escape, and the lengths count the escaped bytes.
    #[test]
    fn an_ordinary_address_keeps_the_key_it_was_written_under() {
        assert_eq!(
            cache_key("5599@s.whatsapp.net", "123"),
            "a-25-5599.40s.2Ewhatsapp.2Enet-3-123"
        );
    }
}
