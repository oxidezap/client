//! What the user has allowed each plugin to do.
//!
//! A plugin declares what it wants during `oxi_init`; nothing is granted by
//! declaring it. Without this file the declaration was a label rather than a
//! decision — copying a `.wasm` into a folder and restarting was consent, and
//! the sentence a user reads in Settings appeared only after the plugin had
//! already sent its first message.
//!
//! Two properties make it worth its size:
//!
//! * It lives beside the daemon's own state, **not** in the plugin's
//!   key-value store. A plugin that could write its own approval has none.
//! * Approval is recorded against the exact mask that was approved. A plugin
//!   that comes back asking for more than it was allowed is not partly
//!   approved — it is unapproved again, because the sentence the user agreed
//!   to is no longer the sentence being asked.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use oxidezap_plugin_abi as abi;

use crate::store::Backing;

/// How large this document may be to be worth reading.
///
/// A mask per plugin id, at most [`crate::MAX_PLUGINS`] of them. Generous by
/// two orders of magnitude, and the reason it is bounded at all is that
/// reading it is an allocation made from whatever is in the state directory.
const MAX_BYTES: usize = 64 * 1024;

/// What a plugin holds, given what it asked for and what was agreed to.
///
/// All or nothing on the gated half, deliberately. A plugin that comes back
/// wanting *more* than was agreed to is not partly approved: the user said
/// yes to a sentence, and this is a different sentence. One that wants less
/// keeps working, because a narrower sentence is covered by a wider answer.
#[must_use]
pub fn effective(requested: i64, approved: i64) -> i64 {
    let gated = requested & abi::caps::NEEDS_APPROVAL;
    if gated & !approved == 0 {
        requested
    } else {
        requested & !abi::caps::NEEDS_APPROVAL
    }
}

/// What the file is called, in the one place that has to agree with the
/// account reset's fallback and with the `kv-` prefix that keeps a plugin id
/// from naming it.
pub const FILE_NAME: &str = "approvals.json";

/// The approved mask per plugin id, mirrored to a document that outlives the
/// process.
pub struct Approvals {
    /// Where the answers are kept.
    ///
    /// Behind a lock because a reload replaces it: a page's storage handle is
    /// stamped, and the generation that takes a fresh one retires the handle
    /// this was opened with — so an approval written after a reload would be
    /// refused by a store nobody had told about the swap. See
    /// [`Approvals::rebind`].
    store: Mutex<Arc<dyn Backing>>,
    granted: Mutex<BTreeMap<String, i64>>,
}

impl Approvals {
    /// Read what has been allowed, or start with nothing allowed.
    ///
    /// A document that cannot be read starts empty, which is the safe
    /// direction: the cost is a prompt the user has answered before, and the
    /// alternative is granting something nobody can account for.
    #[must_use]
    pub fn open(store: Arc<dyn Backing>) -> Self {
        let granted = store
            .read(FILE_NAME, MAX_BYTES)
            .and_then(|bytes| {
                serde_json::from_slice(&bytes)
                    .map_err(|e| {
                        log::warn!(
                            "{} is unreadable ({e}); every plugin starts unapproved",
                            store.describe(FILE_NAME)
                        );
                    })
                    .ok()
            })
            .unwrap_or_default();
        Self {
            store: Mutex::new(store),
            granted: Mutex::new(granted),
        }
    }

    /// Point at a new store, and make sure it holds what is in hand.
    ///
    /// What a reload does at the moment it installs a generation. Three
    /// things, and each is a case that bites without it.
    ///
    /// The map is *not* re-read: this host is the only thing that ever writes
    /// the document — a plugin that could write its own approval would have
    /// none — so what is in memory is what is on disk, and re-reading would
    /// open a window for a stale file to undo an answer given during the
    /// load.
    ///
    /// A store that cannot keep answers clears them instead. A state
    /// directory that was usable at startup and is refused now — replaced by
    /// a symlink, or no longer private — must leave every plugin unapproved
    /// until somebody says otherwise, which is the whole of what
    /// `usable_state_dir` refusing is for; keeping the masks and writing them
    /// nowhere would be the opposite of it.
    ///
    /// And what is in hand is written through the new store, because a page's
    /// old handle went stale the moment the reload took a fresh one: an
    /// answer given during the load changed the map and was refused by the
    /// store it was written through, so without this the document still holds
    /// the old grant and hands it back on the next start.
    pub fn rebind(&self, store: Arc<dyn Backing>) {
        let keeps = store.keeps_answers();
        let mut granted = self.lock();
        let kept = {
            let mut held = self
                .store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let was = held.keeps_answers();
            *held = store;
            was
        };
        if keeps {
            self.flush(&granted);
        } else if kept && !granted.is_empty() {
            // Only on the way *down*. What has to be forgotten is an answer
            // given against a directory that has since been refused — the
            // grant outliving the trust it was recorded under. A host that
            // never had a directory is a different sentence: it may still be
            // told yes, and that answer holds for this session, which is
            // exactly what the refusal promises. Clearing on every rebind
            // took those away too, so a reload silently revoked everything
            // somebody had allowed since the daemon started.
            log::warn!(
                "plugin permissions can no longer be recorded; every plugin is unapproved \
                 until it is allowed again"
            );
            granted.clear();
        }
    }

    /// What this plugin may actually do, given what it asked for.
    #[must_use]
    pub fn granted(&self, id: &str, requested: i64) -> i64 {
        effective(requested, self.approved(id))
    }

    /// Whether everything this plugin asked for that needs agreeing to has
    /// been agreed to.
    ///
    /// A plugin that asks for nothing gated is approved by definition: there
    /// is no sentence, and a prompt with nothing in it only teaches people to
    /// dismiss prompts.
    #[must_use]
    pub fn is_approved(&self, id: &str, requested: i64) -> bool {
        self.granted(id, requested) == requested
    }

    /// The raw mask that was agreed to, whatever the plugin now asks for.
    #[must_use]
    pub fn approved(&self, id: &str) -> i64 {
        self.lock().get(id).copied().unwrap_or(0)
    }

    /// Record the user's answer, and return the mask to hand the plugin.
    ///
    /// The lock is held across the change *and* the write, so what reaches
    /// the file is what was decided last. Releasing it in between let two
    /// clients' answers land out of order — an older grant finishing after a
    /// newer revocation would leave the running daemon revoked and the file
    /// saying the opposite, so the next start handed the capability back.
    pub fn set(&self, id: &str, requested: i64, approved: bool) -> i64 {
        let agreed = requested & abi::caps::NEEDS_APPROVAL;
        let mut granted = self.lock();
        let before = granted.get(id).copied();
        if approved {
            granted.insert(id.to_owned(), agreed);
        } else {
            granted.remove(id);
        }
        if !self.flush(&granted) && approved {
            // A grant that could not be written down is not a grant. The
            // caller hands this mask straight to the running plugin, so
            // returning it would show the capability as allowed while nothing
            // recorded it — and the next start would ask again, which is the
            // grant-before-enable ordering broken in the visible direction.
            // A *withdrawal* keeps its in-memory effect whatever the disk
            // did: failing closed is the point, and `flush` has already
            // removed the file rather than leave a stale grant on it.
            match before {
                Some(mask) => granted.insert(id.to_owned(), mask),
                None => granted.remove(id),
            };
            return granted.get(id).copied().unwrap_or(0);
        }
        granted.get(id).copied().unwrap_or(0)
    }

    /// Write the whole map out.
    ///
    /// Takes the guard rather than re-locking: the caller's mutation and this
    /// write are one step, and two clients answering at once must not be able
    /// to leave the document disagreeing with the running host.
    ///
    /// Answers whether what is held is now stored. `true` for a store with
    /// nowhere to write: there is nothing it could fail at.
    fn flush(&self, granted: &BTreeMap<String, i64>) -> bool {
        let Ok(json) = serde_json::to_vec(granted) else {
            return false;
        };
        let store = Arc::clone(
            &self
                .store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let Err(e) = store.write(FILE_NAME, &json) else {
            return true;
        };
        // Fail closed. Leaving the previous document is the tempting answer
        // and it is the wrong one: the write that most matters is a
        // *withdrawal*, and a stale document that outlives one hands the
        // capability back on the next start — revoked in this host, granted
        // in the one after it. Removing it costs the answers the user has
        // given, which they are asked for again; keeping it costs a
        // permission nobody agreed to.
        log::warn!(
            "cannot write {}: {e}. Every plugin permission will be asked for again.",
            store.describe(FILE_NAME)
        );
        // And say so when even that does not land. It is the whole of what
        // this branch is for: the caller keeps a withdrawal's in-memory
        // effect whatever the store did, so a removal that failed silently
        // would leave the plugin drawn as revoked here and granted again on
        // the next start. Nothing can undo the revocation — that would be the
        // worse answer — so what is left is saying it out loud.
        if let Err(e) = store.remove(FILE_NAME) {
            log::error!(
                "and {} could not be removed either ({e}); a permission withdrawn now may be \
                 granted again on the next start",
                store.describe(FILE_NAME)
            );
        }
        false
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, i64>> {
        // A plain map of owned values: nothing behind this lock can be torn.
        self.granted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Files;
    use oxidezap_plugin_abi as abi;
    use std::path::{Path, PathBuf};

    /// The store these tests are about: files in a directory of their own.
    fn files(dir: &Path) -> Arc<dyn Backing> {
        Arc::new(Files::at(dir))
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "oxidezap-approvals-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("a writable temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The whole point: dropping a file in the folder grants nothing.
    #[test]
    fn a_plugin_starts_unable_to_act_on_the_account() {
        let dir = TempDir::new("fresh");
        let a = Approvals::open(files(&dir.0));
        assert_eq!(a.granted("autoreply", abi::caps::SEND), 0);
        assert!(!a.is_approved("autoreply", abi::caps::SEND));
    }

    /// Drawing, its own settings and its own timer are the plugin's own
    /// business and take effect on declaration — otherwise it could not draw
    /// the panel that explains what it is asking for.
    #[test]
    fn what_a_plugin_does_only_to_itself_needs_no_answer() {
        let dir = TempDir::new("ungated");
        let a = Approvals::open(files(&dir.0));
        let own = abi::caps::UI | abi::caps::STORAGE | abi::caps::TIMERS;
        assert_eq!(a.granted("p", own), own);
        assert!(a.is_approved("p", own));
    }

    /// And a mixed request keeps the ungated half while the rest waits.
    #[test]
    fn an_unapproved_plugin_keeps_only_what_it_does_to_itself() {
        let dir = TempDir::new("mixed");
        let a = Approvals::open(files(&dir.0));
        let asked = abi::caps::SEND | abi::caps::UI;
        assert_eq!(a.granted("p", asked), abi::caps::UI);
        assert!(!a.is_approved("p", asked));
    }

    /// And one that asks for nothing needs no prompt: a permission dialog
    /// with nothing in it teaches people to dismiss permission dialogs.
    #[test]
    fn a_plugin_that_asks_for_nothing_is_approved_already() {
        let dir = TempDir::new("nothing");
        let a = Approvals::open(files(&dir.0));
        assert!(a.is_approved("watcher", 0));
    }

    #[test]
    fn an_answer_survives_a_restart() {
        let dir = TempDir::new("persist");
        let wanted = abi::caps::SEND | abi::caps::UI;
        {
            let a = Approvals::open(files(&dir.0));
            a.set("autoreply", wanted, true);
        }
        let a = Approvals::open(files(&dir.0));
        assert_eq!(a.granted("autoreply", wanted), wanted);
        assert!(a.is_approved("autoreply", wanted));
    }

    #[test]
    fn withdrawing_takes_everything_back() {
        let dir = TempDir::new("withdraw");
        let a = Approvals::open(files(&dir.0));
        a.set("autoreply", abi::caps::SEND, true);
        a.set("autoreply", abi::caps::SEND, false);
        assert_eq!(a.granted("autoreply", abi::caps::SEND), 0);
    }

    /// The reason approval is recorded against a mask rather than a flag: an
    /// update that quietly wants more is not covered by the answer given to
    /// the version that wanted less.
    #[test]
    fn a_plugin_that_wants_more_than_it_was_allowed_gets_none_of_it() {
        let dir = TempDir::new("widened");
        let a = Approvals::open(files(&dir.0));
        a.set("autoreply", abi::caps::SEND, true);

        let widened = abi::caps::SEND | abi::caps::MARK_READ;
        assert_eq!(
            a.granted("autoreply", widened),
            0,
            "not even the half that was allowed"
        );
        assert!(!a.is_approved("autoreply", widened));
    }

    /// Narrowing is the other direction and must keep working: a plugin that
    /// drops a capability should not need to be re-approved.
    #[test]
    fn a_plugin_that_wants_less_keeps_working() {
        let dir = TempDir::new("narrowed");
        let a = Approvals::open(files(&dir.0));
        a.set("autoreply", abi::caps::SEND | abi::caps::MARK_READ, true);
        assert_eq!(a.granted("autoreply", abi::caps::SEND), abi::caps::SEND);
        assert!(a.is_approved("autoreply", abi::caps::SEND));
    }

    #[test]
    fn plugins_do_not_share_an_answer() {
        let dir = TempDir::new("separate");
        let a = Approvals::open(files(&dir.0));
        a.set("one", abi::caps::SEND, true);
        assert_eq!(a.granted("two", abi::caps::SEND), 0);
    }

    /// The write that matters most is a withdrawal, so a flush that cannot
    /// land must not leave a file that outlives it: revoked here and granted
    /// again on the next start is the one outcome worse than asking twice.
    #[test]
    fn a_failed_write_does_not_leave_a_grant_behind() {
        let dir = TempDir::new("fail-closed");
        let a = Approvals::open(files(&dir.0));
        a.set("autoreply", abi::caps::SEND, true);
        assert!(dir.0.join("approvals.json").exists());

        // A directory where the file has to go: the rename cannot replace it,
        // which is the shape of any write that fails at the last step.
        std::fs::remove_file(dir.0.join("approvals.json")).expect("writable");
        std::fs::create_dir(dir.0.join("approvals.json")).expect("writable");
        a.set("autoreply", abi::caps::SEND, false);

        let reread = Approvals::open(files(&dir.0));
        assert_eq!(
            reread.approved("autoreply"),
            0,
            "nothing survives a flush that could not land"
        );
    }

    /// The caller hands what this returns straight to the running plugin, so
    /// a grant nobody could write down must not come back as one: Settings
    /// would show the capability allowed while the next start asked again.
    #[test]
    fn a_grant_that_cannot_be_written_is_not_granted() {
        let dir = TempDir::new("grant-unwritable");
        let a = Approvals::open(files(&dir.0));
        // A directory where the file has to go, so the rename cannot land.
        std::fs::create_dir(dir.0.join("approvals.json")).expect("writable");

        assert_eq!(
            a.set("autoreply", abi::caps::SEND, true),
            0,
            "nothing was recorded, so nothing is allowed"
        );
        assert!(!a.is_approved("autoreply", abi::caps::SEND));
    }

    #[test]
    fn a_corrupt_file_grants_nothing_rather_than_everything() {
        let dir = TempDir::new("corrupt");
        std::fs::write(dir.0.join("approvals.json"), b"{not json").expect("writable");
        let a = Approvals::open(files(&dir.0));
        assert_eq!(a.granted("autoreply", abi::caps::SEND), 0);
    }
}
