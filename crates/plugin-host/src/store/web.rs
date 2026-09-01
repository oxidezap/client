//! The origin's own key-value storage.
//!
//! What is peculiar to the page is here: the page rebuilds its whole service
//! in one agent and cannot join a plugin's task, so a handle carries a stamp
//! and a superseded one may neither write nor remove. See [`LIVE`], which is
//! the whole of that rule.

use super::Backing;

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
pub struct Origin {
    /// Which handle this is. See [`LIVE`].
    stamp: u64,
}

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
        // The same question `write` asks, and asking it here is what makes
        // that guard whole rather than a way in. A superseded handle's write
        // is refused *because* it is superseded, and the one caller that acts
        // on a refused write answers it by removing the document instead — so
        // leaving this unguarded let a stale host delete the live host's
        // approvals, which is every plugin unapproved on the next start on
        // the say-so of a host that has already been replaced. A handle that
        // may not write may not delete.
        if !self.live() {
            return Err("this store belongs to a host that has been replaced".to_owned());
        }
        let storage = Self::local().ok_or_else(|| "this page has no storage".to_owned())?;
        storage
            .remove_item(&Self::key(name))
            .map_err(|e| format!("{e:?}"))
    }

    fn describe(&self, name: &str) -> String {
        format!("{} in this browser's storage", Self::key(name))
    }
}

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
static LIVE: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);
