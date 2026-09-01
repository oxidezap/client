//! Files in a directory the host owns.
//!
//! Everything the desktop half has to be careful about is here and nowhere
//! else: a directory that another local account could once write is one whose
//! documents are suspect, and a rename or an unlink is a directory entry that
//! is not persisted until the directory itself is flushed. See [`Backing`] for
//! what the two callers above this rely on.

use std::path::{Path, PathBuf};

use super::Backing;

/// Files in one directory. Native only; a page has no directory to name.
pub struct Files(PathBuf);

impl Files {
    /// Documents under `dir`, which the caller has already made private.
    #[must_use]
    pub fn at(dir: &Path) -> Self {
        Self(dir.to_path_buf())
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Backing for Files {
    fn read(&self, name: &str, max: usize) -> Option<Vec<u8>> {
        let path = self.path(name);
        // The same question the state directory is asked, for the weaker but
        // real version of the same reason: this directory may have been open
        // before the host closed it, so a document in it can be one another
        // local account wrote. Removed rather than merely ignored — leaving
        // it hands the next start the same forged answer.
        if path.exists() && !crate::only_this_user_can_write(&path) {
            log::warn!(
                "{} could have been written by another user on this machine; starting empty",
                path.display()
            );
            let _ = std::fs::remove_file(&path);
            return None;
        }
        // Bounded before it is read, not after: reading a planted file to
        // discover how big it is would be the allocation the bound refuses.
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > max as u64) {
            log::warn!(
                "{} is larger than it may be; starting empty",
                path.display()
            );
            return None;
        }
        match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                log::warn!("cannot read {} ({e}); starting empty", path.display());
                None
            }
        }
    }

    fn write(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        let path = self.path(name);
        // Unique per process and thread. A fixed name is one two daemons
        // sharing a state directory both write, so one can rename a file the
        // other is still filling.
        let temp = path.with_extension(format!(
            "{}.{:?}.tmp",
            std::process::id(),
            std::thread::current().id()
        ));
        let landed = crate::write_private(&temp, bytes)
            .and_then(|()| std::fs::rename(&temp, &path))
            // The rename is metadata, and syncing the file did not persist
            // it: an answer that reported success while the entry was still
            // only in memory is a withdrawal the next start hands back.
            .and_then(|()| match path.parent() {
                Some(dir) => crate::sync_dir(dir),
                None => Ok(()),
            });
        match landed {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                Err(e.to_string())
            }
        }
    }

    fn remove(&self, name: &str) -> Result<(), String> {
        let path = self.path(name);
        match std::fs::remove_file(&path) {
            // The unlink is a directory entry like a rename, and just as
            // unpersisted until the directory is flushed: a document removed
            // to withhold a grant is one that can come back. Counted as part
            // of the removal rather than logged beside it, for the reason the
            // rename's sync is.
            Ok(()) => match path.parent() {
                Some(dir) => crate::sync_dir(dir).map_err(|e| e.to_string()),
                None => Ok(()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn describe(&self, name: &str) -> String {
        self.path(name).display().to_string()
    }
}
