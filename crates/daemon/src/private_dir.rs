//! One answer to "is this directory ours, and ours alone".
//!
//! The daemon writes two things under its own directory that carry the
//! account: the socket, which is control of the session, and the media cache,
//! which is a copy of every photo, video and document that has passed through
//! it. Under `XDG_RUNTIME_DIR` the parent is already private per user; the
//! `TMPDIR` fallback is not, and neither is a directory an older version left
//! behind at a looser mode. So both go through here rather than one being
//! checked carefully and the other being created blindly.

use std::path::Path;

use anyhow::{Context, Result};

/// What the directory was when this process found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Found {
    /// Created now, or already private.
    Private,
    /// Ours, but reachable by somebody else until a moment ago. Tightening
    /// closes the door; it says nothing about what is already inside.
    WasOpen,
}

/// Create `dir` private, or establish that an existing one is safe to use.
///
/// Refuses anything that is not a real directory owned by this user, because
/// the two candidates for what else it could be are a symlink pointing
/// somewhere its author can read and a directory somebody else created at a
/// path they could predict. `symlink_metadata`, not `metadata`: the latter
/// answers for the target and misses exactly that substitution.
///
/// A directory that is ours but too permissive is tightened rather than
/// refused — the common case is an earlier version of this daemon — and the
/// caller is told, because a `chmod` now only closes the door behind whatever
/// is already in the room.
#[cfg(unix)]
pub(crate) fn prepare(dir: &Path, purpose: &str) -> Result<Found> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(Found::Private),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
    }

    let meta =
        std::fs::symlink_metadata(dir).with_context(|| format!("inspecting {}", dir.display()))?;
    if !meta.is_dir() {
        anyhow::bail!(
            "{} exists but is not a directory; refusing to keep {purpose} there",
            dir.display()
        );
    }
    if meta.uid() != current_uid() {
        anyhow::bail!(
            "{} is owned by uid {}, not by us; refusing to keep {purpose} there",
            dir.display(),
            meta.uid()
        );
    }

    let mode = meta.permissions().mode() & 0o777;
    if mode == 0o700 {
        return Ok(Found::Private);
    }
    log::warn!("tightening {} from {mode:o} to 700", dir.display());
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restricting {}", dir.display()))?;
    Ok(Found::WasOpen)
}

/// Windows has no mode to read; what stands in for it is where the directory
/// is, under the profile's own ACL.
#[cfg(not(unix))]
pub(crate) fn prepare(dir: &Path, _purpose: &str) -> Result<Found> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(Found::Private)
}

/// Remove the entries in `dir` that this daemon could not have written.
///
/// For a directory that was reachable by another local account: tightening it
/// says nothing about what was left inside while it was open, and every name
/// under it is predictable — the socket, the lock, and media keys derived
/// from content the account already published.
///
/// Two kinds go. A symlink, because it is never ours under any of those names
/// and following one is how a planted entry becomes a file the daemon writes
/// through. And anything owned by somebody else, which is the other half of
/// the same sentence and the one with teeth: a `daemon.lock` another account
/// holds open is a daemon that never starts, and a `daemon.sock` they are
/// listening on is a bind that never happens and a front end that connects to
/// them instead. Owned by *us* is the test that keeps this off a live sibling
/// daemon's own socket and lock, which are the two entries here worth
/// protecting.
///
/// Reported rather than fatal: a directory that cannot be swept is one whose
/// contents were about to be trusted, and the caller decides which of those
/// is worse.
#[cfg(unix)]
pub(crate) fn drop_foreign_entries(dir: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        // About the link rather than through it, for the reason `prepare`
        // reads it that way: the target's owner says nothing about who may
        // put a different file there.
        let meta = std::fs::symlink_metadata(&path)
            .with_context(|| format!("inspecting {}", path.display()))?;
        let ours = !meta.file_type().is_symlink() && meta.uid() == current_uid();
        if ours {
            continue;
        }
        log::warn!(
            "removing {}, which this daemon did not put there",
            path.display()
        );
        let removed = if meta.file_type().is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        removed.with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn drop_foreign_entries(_dir: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "oxidezap-private-dir-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A directory another local account could reach is one they could put a
    /// symlink in, under a name this daemon is about to write through — and
    /// tightening the mode only closes the door behind it.
    #[test]
    fn a_directory_that_was_open_does_not_keep_what_was_left_in_it() {
        let dir = scratch("open");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let planted = dir.join("media");
        std::os::unix::fs::symlink(std::env::temp_dir(), &planted).unwrap();
        let ours = dir.join("daemon.lock");
        std::fs::write(&ours, b"").unwrap();

        assert_eq!(prepare(&dir, "the socket").unwrap(), Found::WasOpen);
        drop_foreign_entries(&dir).unwrap();

        assert!(!planted.exists() && planted.symlink_metadata().is_err());
        assert!(ours.exists(), "what this daemon writes is left alone");
        // Including the two entries that are not symlinks and stop a daemon
        // dead: a lock somebody else holds, and a socket they are listening
        // on. Ours by uid is what tells them apart, so a live sibling
        // daemon's own files are never the ones removed.
        assert!(dir.join("daemon.lock").exists());
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The substitution the check exists for: `Path::is_dir` follows a link
    /// and answers about the target, which says nothing about who may put a
    /// different file there.
    #[test]
    fn a_symlink_is_not_a_directory_we_may_use() {
        let dir = scratch("link");
        let target = scratch("link-target");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, &dir).unwrap();

        assert!(prepare(&dir, "cached media").is_err());
        std::fs::remove_file(&dir).unwrap();
        std::fs::remove_dir_all(&target).unwrap();
    }

    /// The ordinary case stays ordinary: made private, and second time round
    /// nothing is disturbed.
    #[test]
    fn a_private_directory_is_used_as_it_is() {
        let dir = scratch("private");
        assert_eq!(prepare(&dir, "the socket").unwrap(), Found::Private);
        let ours = dir.join("daemon.sock");
        std::fs::write(&ours, b"").unwrap();
        assert_eq!(prepare(&dir, "the socket").unwrap(), Found::Private);
        assert!(ours.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
