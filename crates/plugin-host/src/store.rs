//! Where a plugin host keeps what has to survive a restart.
//!
//! Two things, and only two: what the user allowed each plugin to do, and
//! whatever a plugin kept for itself. Both are small JSON documents named by
//! the host, which is the whole of why one interface can serve a filesystem
//! and a browser's origin storage — nothing here needs a path, a directory
//! listing or a seek.
//!
//! The rules that made the file version worth its size are properties of the
//! *caller* and stay where they were: approvals never live in a plugin's own
//! store, a plugin id can never name the approvals document because the host
//! prefixes its own with `kv-`, and a failed write of a grant is not a grant.
//! What each implementation owes is narrower — hand back what was written, or
//! nothing.

#[cfg(not(target_family = "wasm"))]
use std::path::{Path, PathBuf};

/// A named document that outlives the process.
///
/// `Send + Sync` because the approvals are read from every thread the daemon
/// answers a request on and written from whichever one the user's answer
/// arrived on. That costs a page nothing: the browser implementation holds a
/// prefix and reaches for its global per call, rather than keeping a JS
/// object it could not share anyway.
pub trait Backing: Send + Sync + 'static {
    /// Read `name`, or `None` when it is not there, unreadable, or larger
    /// than `max`.
    ///
    /// The bound is asked of the stored size where the platform can answer
    /// that without reading — a planted file is an allocation, and reading
    /// one to discover how big it is would be the allocation the bound
    /// exists to refuse.
    fn read(&self, name: &str, max: usize) -> Option<Vec<u8>>;

    /// Replace `name`, atomically enough that a host killed mid-write leaves
    /// either the old document or the new one.
    ///
    /// # Errors
    ///
    /// Whatever the platform said, in words, for the one log line each caller
    /// writes about it.
    fn write(&self, name: &str, bytes: &[u8]) -> Result<(), String>;

    /// Remove `name`, durably. Missing is success.
    ///
    /// # Errors
    ///
    /// Whatever the platform said. Fallible because of the one caller that
    /// acts on it: a withdrawal whose write failed removes the document
    /// instead, and a removal that silently failed would leave the old grant
    /// to be read back on the next start while Settings had already drawn the
    /// plugin as revoked.
    fn remove(&self, name: &str) -> Result<(), String>;

    /// What to call `name` in a log line: a path, or an origin's storage.
    fn describe(&self, name: &str) -> String;
}

/// A store with nowhere to write.
///
/// What a host with no usable state directory gets, and what a test wants.
/// Reads answer nothing and writes succeed, so a plugin keeps its settings
/// for the life of the process and the approvals are asked for again — which
/// is the safe direction for the one of the two that is authority.
pub struct Nowhere;

impl Backing for Nowhere {
    fn read(&self, _name: &str, _max: usize) -> Option<Vec<u8>> {
        None
    }

    fn write(&self, _name: &str, _bytes: &[u8]) -> Result<(), String> {
        Ok(())
    }

    fn remove(&self, _name: &str) -> Result<(), String> {
        Ok(())
    }

    fn describe(&self, name: &str) -> String {
        format!("{name} (kept in memory only)")
    }
}

/// Files in one directory. Native only; a page has no directory to name.
#[cfg(not(target_family = "wasm"))]
pub struct Files(PathBuf);

#[cfg(not(target_family = "wasm"))]
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

#[cfg(not(target_family = "wasm"))]
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

/// The origin's own key-value storage. Web only.
///
/// `localStorage` and not IndexedDB, and the choice is about *shape* rather
/// than about size: both documents here are small, and both are read and
/// written from inside a synchronous wasm call — a plugin sets a key and the
/// host commits when the call returns, with nowhere to await. An
/// asynchronous store would have to be mirrored in memory and written behind
/// the caller's back, which is a second copy of the thing this trait exists
/// to avoid. It is per origin, survives the tab, and is the same place the
/// window keeps its own preferences.
///
/// Not where a plugin's *module* lives: that is megabytes and belongs in
/// OPFS. What is here is a permission answer and a settings map.
#[cfg(target_family = "wasm")]
pub struct Origin {
    /// Which handle this is. See [`LIVE`].
    stamp: u64,
}

#[cfg(target_family = "wasm")]
impl Origin {
    /// Documents under [`PREFIX`], for the host being built now.
    ///
    /// Taking one retires every handle given out before it. See [`LIVE`].
    #[must_use]
    pub fn storage() -> Self {
        Self {
            stamp: LIVE.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1,
        }
    }

    fn key(name: &str) -> String {
        format!("{PREFIX}{name}")
    }

    /// Whether this handle is the one the page is using now.
    fn live(&self) -> bool {
        self.stamp == LIVE.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The origin's store, or `None` where the browser refuses it — a
    /// blocked third-party context, or a mode with storage switched off.
    fn local() -> Option<web_sys::Storage> {
        web_sys::window()?.local_storage().ok().flatten()
    }
}

#[cfg(target_family = "wasm")]
impl Backing for Origin {
    fn read(&self, name: &str, max: usize) -> Option<Vec<u8>> {
        let held = Self::local()?.get_item(&Self::key(name)).ok().flatten()?;
        // Asked of what came back rather than before it, which is the honest
        // difference from a file: `getItem` has already allocated the string
        // by the time anything here can look at it. The bound still does its
        // job — a document past it is refused rather than parsed — and what
        // it cannot do is refuse the read, which is bounded instead by the
        // browser's own few megabytes per origin.
        if held.len() > max {
            log::warn!(
                "{} is larger than it may be; starting empty",
                Self::key(name)
            );
            return None;
        }
        Some(held.into_bytes())
    }

    fn write(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        // Nothing more, once this handle has been superseded. This is the
        // half a page cannot order any other way: a desktop joins every
        // plugin's thread before it replaces the host, so a plugin's last
        // settings write has already happened — and a page cannot join a task
        // on its own loop, so a plugin whose worker has not been polled since
        // the shutdown flag went up still has that write in front of it.
        //
        // Two things it would land on, and the stamp answers both with one
        // rule. After a wipe it would recreate the departed account's data
        // under whoever pairs next. After an ordinary reconnection — no wipe
        // at all — it would put the old host's in-memory settings over what
        // the new host has already written, since a page rebuilds the whole
        // service in the same agent.
        //
        // *Superseded*, and not "any host has ever gone": a latch would leave
        // the new host unable to write anything for the rest of the tab's
        // life — grants rolled back, settings lost — while the tasks it was
        // aimed at are the old host's. See [`LIVE`].
        if !self.live() {
            return Err("this store belongs to a host that has been replaced".to_owned());
        }
        let held = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
        let storage = Self::local().ok_or_else(|| "this page has no storage".to_owned())?;
        // One call, and it is atomic in the sense that matters: a `setItem`
        // either replaces the value or throws, so there is no torn document
        // and no temporary name to clean up. The quota is the failure to
        // expect, and it arrives here rather than being discovered later.
        storage
            .set_item(&Self::key(name), held)
            .map_err(|e| format!("{e:?}"))
    }

    fn remove(&self, name: &str) -> Result<(), String> {
        let storage = Self::local().ok_or_else(|| "this page has no storage".to_owned())?;
        storage
            .remove_item(&Self::key(name))
            .map_err(|e| format!("{e:?}"))
    }

    fn describe(&self, name: &str) -> String {
        format!("{} in this browser's storage", Self::key(name))
    }
}

#[cfg(target_family = "wasm")]
impl Origin {
    /// Remove everything this origin holds for plugins.
    ///
    /// The account leaving, which on a desktop is a directory being deleted.
    /// Both documents go: the approvals because a permission must not outlive
    /// the account that granted it — a plugin with the same id and mask would
    /// otherwise be allowed to act under consent given for an account that no
    /// longer exists — and each plugin's settings because they are that
    /// account's data, an autoreply's list of who it has already answered
    /// being a list of people.
    ///
    /// What stays is the modules themselves, exactly as the desktop keeps its
    /// plugin folder across a reset: what the user installed is not the
    /// account's, and reinstalling it is not what "pair again" should mean.
    ///
    /// Answers whether the storage was there to be cleared. `false` is the
    /// same refusal the desktop makes when it cannot remove the file: the
    /// caller wipes the credentials only once this has said yes.
    #[must_use]
    pub fn forget_all() -> bool {
        // Moved on before anything is removed, so a write racing this one is
        // refused rather than landing behind it. See `Backing::write`.
        LIVE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let Some(storage) = Self::local() else {
            // Not "there was nothing to retire". This browser refused the
            // store *now*, and refusing it is not the same fact as its being
            // empty — a blocked storage context can be unblocked, and the
            // approvals this call was supposed to remove would then be read
            // back for whoever paired in the meantime, with the same id and
            // the same mask allowed to act under consent nobody in that
            // account gave. Nothing here can tell an origin that never held
            // one from an origin whose storage is shut, so the only honest
            // answer is the one that fails closed: the caller refuses the
            // wipe and says so, and the old account stays intact — a state
            // somebody can act on again.
            log::error!(
                "this page has no storage to retire the plugins' permissions in; the wipe \
                 cannot go ahead"
            );
            return false;
        };
        // Collected before anything is removed: `key(i)` is an index into a
        // list this loop is about to change under itself.
        let Ok(count) = storage.length() else {
            return false;
        };
        let mut ours = Vec::new();
        for i in 0..count {
            // An index that throws is not an index that holds nothing. A
            // browser failing part-way through its own enumeration would
            // otherwise have this skip the entry and answer success, and the
            // entry it skipped can be `approvals.json` — the one document
            // whose survival lets a plugin act on the next account under
            // consent given for this one. Refused whole, like every other
            // half-answer here.
            let Ok(key) = storage.key(i) else {
                log::error!(
                    "this page's storage failed while being listed; the plugins' permissions \
                     cannot be shown to be retired, so the wipe cannot go ahead"
                );
                return false;
            };
            // A `None` is the list having shrunk under this loop, which is
            // not a failure: nothing removes from this storage but the lines
            // below, and they have not run yet.
            if let Some(key) = key
                && key.starts_with(PREFIX)
            {
                ours.push(key);
            }
        }
        ours.iter().all(|key| storage.remove_item(key).is_ok())
    }
}

/// What every document this host keeps is named under.
#[cfg(target_family = "wasm")]
const PREFIX: &str = "oxidezap.plugin.";

/// Which handle the page is writing through now.
///
/// Bumped by [`Origin::storage`], so taking a handle retires every one given
/// out before it, and by [`Origin::forget_all`], so a departure retires the
/// handle that was live when it happened. One counter for both, because they
/// are one question: is this store still the page's?
///
/// Page-wide rather than state on the store, because what it records is about
/// the page — a host has been replaced — and every handle from before it is
/// stale whoever is holding it.
///
/// Through `portable-atomic`, like every other 64-bit atomic here: a page is
/// a 32-bit target, and there is no native 64-bit atomic on one.
#[cfg(target_family = "wasm")]
static LIVE: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);
