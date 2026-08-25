//! Where the socket lives and what version speaks over it.

use std::path::PathBuf;

/// Bumped whenever a frame changes shape in a way an older peer would
/// misread. The daemon refuses a mismatch rather than guessing.
///
/// 3: the session's own event stream, opt-in at the hello, plus the requests
/// a full front end needs to drive it — audio, typing, calls, downloads and
/// `ForgetSession`. Media travels through [`media_path`] rather than the
/// socket, and `SendText` gained the local id a client that draws the message
/// before it is sent has to know.
///
/// 2: `Pairing` carries a [`PairingCode`] per credential rather than two bare
/// strings, `MessagePreview` names the message it describes, `MarkRead`
/// echoes that name back, `ShowWindow`, `SendFailed`, `Refused` and
/// `TooManyClients` were added, and `Unsupported` was removed once every
/// request the protocol defines became one the daemon acts on. A v1 peer
/// would misparse the first three and not recognise the rest.
///
/// [`PairingCode`]: crate::PairingCode
pub const PROTOCOL_VERSION: u32 = 3;

const SOCKET_NAME: &str = "daemon.sock";
const DIR_NAME: &str = "oxidezap";
const MEDIA_DIR: &str = "media";

/// Path of the daemon's listening socket.
///
/// Prefers `XDG_RUNTIME_DIR`, which is per-user, mode 0700 and cleared on
/// logout: a socket that grants control of a WhatsApp session does not belong
/// in a world-writable `/tmp`. Falls back to `TMPDIR` with the uid in the
/// directory name, so two users on one machine cannot collide or reach each
/// other's daemon.
///
/// Returns `None` when neither is usable rather than inventing a path, so the
/// caller reports it instead of listening somewhere unexpected.
#[must_use]
pub fn socket_path() -> Option<PathBuf> {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(runtime).join(DIR_NAME).join(SOCKET_NAME));
    }

    let tmp = std::env::var_os("TMPDIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));

    // Only reachable when XDG_RUNTIME_DIR is unset, which is unusual on a
    // desktop; the uid keeps the fallback per-user anyway.
    let uid = uid_suffix();
    Some(tmp.join(format!("{DIR_NAME}-{uid}")).join(SOCKET_NAME))
}

/// Where a media payload with this cache key lives.
///
/// A photo is megabytes and the socket carries newline-delimited JSON, so
/// media never travels as a frame: the side that has the bytes writes them
/// here and the other side reads the file. Both derive the path from the same
/// place the socket comes from, so they cannot disagree about it.
///
/// The directory is the daemon's, mode 0700 like its parent, and both
/// processes run as the same user — a client writing a voice note into it is
/// putting a file in its own scratch space, not reaching into the daemon.
///
/// Returns `None` for the same reason [`socket_path`] does, and for a key that
/// is not a plain name: a key is echoed from a peer, and one carrying a
/// separator or a leading dot would name a file outside the cache.
#[must_use]
pub fn media_path(key: &str) -> Option<PathBuf> {
    let sane = !key.is_empty()
        && key.len() <= 128
        && !key.starts_with('.')
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    sane.then(media_dir).flatten().map(|dir| dir.join(key))
}

/// The directory [`media_path`] resolves into.
#[must_use]
pub fn media_dir() -> Option<PathBuf> {
    Some(socket_path()?.parent()?.join(MEDIA_DIR))
}

#[cfg(unix)]
fn uid_suffix() -> String {
    // rustix rather than a hand-rolled `extern "C"`: the same syscall with no
    // `unsafe` at this call site, from a crate already in the tree.
    rustix::process::getuid().as_raw().to_string()
}

#[cfg(not(unix))]
fn uid_suffix() -> String {
    std::env::var("USERNAME").unwrap_or_else(|_| "user".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_dir_wins_when_set() {
        // Not using the process environment: these tests run in parallel and
        // env mutation is process-wide.
        let path = PathBuf::from("/run/user/1000")
            .join(DIR_NAME)
            .join(SOCKET_NAME);
        assert_eq!(path.file_name().unwrap(), SOCKET_NAME);
        assert!(path.starts_with("/run/user/1000"));
    }

    /// A cache key is echoed from a peer. One carrying a separator or a
    /// leading dot names a file outside the cache, and the daemon writes
    /// there as the user who owns the session.
    #[test]
    fn a_key_that_could_escape_the_cache_resolves_to_nothing() {
        for key in [
            "../../.ssh/authorized_keys",
            "sub/dir",
            ".hidden",
            "",
            "/etc/passwd",
        ] {
            assert!(media_path(key).is_none(), "{key} was allowed");
        }
    }

    #[test]
    fn an_ordinary_key_lands_inside_the_cache() {
        let dir = media_dir().expect("a cache directory is always derivable");
        let path = media_path("a1b2c3.jpg").expect("a plain name is a key");
        assert_eq!(path.parent(), Some(dir.as_path()));
        assert!(path.starts_with(dir));
    }

    #[test]
    fn a_path_is_always_produced() {
        let path = socket_path().expect("a path is always derivable");
        assert_eq!(path.file_name().unwrap(), SOCKET_NAME);
        assert!(
            path.parent()
                .is_some_and(|p| p.to_string_lossy().contains(DIR_NAME)),
            "socket sits in its own directory so its permissions are ours to set"
        );
    }
}
