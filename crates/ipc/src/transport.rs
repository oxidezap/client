//! Where the socket lives and what version speaks over it.

use std::path::PathBuf;

/// Bumped whenever a frame changes shape in a way an older peer would
/// misread. The daemon refuses a mismatch rather than guessing.
///
/// 8: the account identity carries its LID as well as its phone number. A
/// chat with your own number can be keyed by either alias, and a client
/// holding only one of them cannot recognise the other.
///
/// 7: `StorageUsage` and `ClearMediaCache`, answered by
/// `DaemonMessage::Storage`. The daemon is the only process that opens the
/// store or writes the media cache, so it is the only one that can measure
/// either.
///
/// 6: `DaemonEvent::CallsChanged` publishes the call state to every front
/// end. The daemon makes some call transitions itself — accepting one brings
/// the media up in the process that owns the microphone — and a second window
/// had no way to hear about them.
///
/// 5: `SendText` carries what it quotes, so a reply is sent as one rather
/// than as a fresh message, and the snapshot names the linked account.
///
/// 4: every request may carry an id, and every answer echoes it. Before that
/// a refused send could only be reported by inventing a failure against the
/// message the client had drawn, and a refused download by nothing at all.
/// The snapshot also carries the whole `CallState` rather than a list of
/// ringing calls, because a call this account placed was never an event and
/// no replay reconstructs it.
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
pub const PROTOCOL_VERSION: u32 = 8;

/// Only a Unix endpoint is a file with a name in a directory.
#[cfg(unix)]
const SOCKET_NAME: &str = "daemon.sock";
const DIR_NAME: &str = "oxidezap";
const MEDIA_DIR: &str = "media";

/// Where the daemon listens and a client connects.
///
/// Two things on Unix and one thing on Windows, which is why it is separate
/// from [`state_dir`]: a Unix socket *is* a filesystem entry beside the
/// daemon's other state, while a Windows named pipe is a name in a namespace
/// of its own and has no directory to sit in.
///
/// Returns `None` when there is nowhere sensible rather than inventing a
/// path, so the caller reports it instead of listening somewhere unexpected.
#[must_use]
pub fn endpoint_path() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        Some(state_dir()?.join(SOCKET_NAME))
    }
    #[cfg(windows)]
    {
        // Named pipes are machine-wide, so the name carries the user: two
        // people signed into one machine must not land on each other's
        // session. The same reason the Unix fallback carries the uid.
        Some(PathBuf::from(format!(
            r"\\.\pipe\{DIR_NAME}-{}",
            user_suffix()?
        )))
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// The directory holding everything the daemon keeps between frames: its
/// startup lock and its media cache.
///
/// On Unix this prefers `XDG_RUNTIME_DIR`, which is per-user, mode 0700 and
/// cleared on logout: a socket that grants control of a WhatsApp session does
/// not belong in a world-writable `/tmp`. It falls back to `TMPDIR` with the
/// uid in the directory name, so two users on one machine cannot collide or
/// reach each other's daemon.
///
/// On Windows it is under `LOCALAPPDATA`, which is already inside the user's
/// profile and so already private to them.
#[must_use]
pub fn state_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let local = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty())?;
        Some(PathBuf::from(local).join(DIR_NAME))
    }

    #[cfg(not(windows))]
    {
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(runtime).join(DIR_NAME));
        }

        let tmp = std::env::var_os("TMPDIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));

        // Only reachable when XDG_RUNTIME_DIR is unset, which is unusual on a
        // desktop; the uid keeps the fallback per-user anyway.
        Some(tmp.join(format!("{DIR_NAME}-{}", user_suffix()?)))
    }
}

/// Where the daemon's startup lock lives.
///
/// A file rather than the socket path with an extension, because on Windows
/// the endpoint is not a file at all.
#[must_use]
pub fn lock_path() -> Option<PathBuf> {
    Some(state_dir()?.join("daemon.lock"))
}

/// Where a media payload with this cache key lives.
///
/// A photo is megabytes and the socket carries newline-delimited JSON, so
/// media never travels as a frame: the side that has the bytes writes them
/// here and the other side reads the file. Both derive the path from the same
/// place, so they cannot disagree about it.
///
/// The directory is the daemon's, and both processes run as the same user — a
/// client writing a voice note into it is putting a file in its own scratch
/// space, not reaching into the daemon.
///
/// Returns `None` for the same reason [`state_dir`] does, and for a key that
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
    Some(state_dir()?.join(MEDIA_DIR))
}

/// What distinguishes one user's daemon from another's on the same machine.
///
/// `None` when the platform will not say, which is a reason to report that
/// there is nowhere to listen rather than to invent a name every user would
/// share.
#[cfg(unix)]
fn user_suffix() -> Option<String> {
    // rustix rather than a hand-rolled `extern "C"`: the same syscall with no
    // `unsafe` at this call site, from a crate already in the tree.
    Some(rustix::process::getuid().as_raw().to_string())
}

#[cfg(windows)]
fn user_suffix() -> Option<String> {
    // The SID, not `USERNAME`. A pipe name is machine-wide and an environment
    // variable is not an identity: two accounts from different domains can
    // share a name, and a process controls its own environment. This is the
    // identity the kernel uses, and the same one the daemon's access-control
    // entry names.
    let sid = crate::windows_user::sid_string().ok()?;
    Some(
        sid.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .take(96)
            .collect(),
    )
}

#[cfg(not(any(unix, windows)))]
fn user_suffix() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `XDG_RUNTIME_DIR` is a Unix idea, and so is the shape it produces.
    #[cfg(unix)]
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

    /// The endpoint is always derivable, and always says which user it
    /// belongs to — the shape differs by platform, the property does not.
    #[test]
    fn an_endpoint_is_always_produced() {
        let path = endpoint_path().expect("an endpoint is always derivable");

        #[cfg(unix)]
        {
            assert_eq!(path.file_name().unwrap(), SOCKET_NAME);
            assert!(
                path.parent()
                    .is_some_and(|p| p.to_string_lossy().contains(DIR_NAME)),
                "the socket sits in its own directory so its permissions are ours to set"
            );
        }
        #[cfg(windows)]
        {
            let name = path.to_string_lossy();
            assert!(name.starts_with(r"\\.\pipe\"), "not a pipe name: {name}");
            assert!(
                name.contains(DIR_NAME),
                "a pipe name is machine-wide, so it has to say whose it is: {name}"
            );
        }
    }

    /// Both live under the same per-user directory, so whatever protects one
    /// protects the other.
    #[test]
    fn the_cache_sits_with_the_daemon_s_other_state() {
        let state = state_dir().expect("a state directory is always derivable");
        assert!(media_dir().is_some_and(|dir| dir.starts_with(&state)));
        assert!(lock_path().is_some_and(|path| path.starts_with(&state)));
    }
}
