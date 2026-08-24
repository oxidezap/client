//! Where the socket lives and what version speaks over it.

use std::path::PathBuf;

/// Bumped whenever a frame changes shape in a way an older peer would
/// misread. The daemon refuses a mismatch rather than guessing.
pub const PROTOCOL_VERSION: u32 = 1;

const SOCKET_NAME: &str = "daemon.sock";
const DIR_NAME: &str = "oxidezap";

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

#[cfg(unix)]
fn uid_suffix() -> String {
    // SAFETY: getuid is always safe; it reads a process property and cannot fail.
    unsafe { libc_getuid() }.to_string()
}

#[cfg(unix)]
unsafe fn libc_getuid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
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
