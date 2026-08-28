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
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use oxidezap_plugin_abi as abi;

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

/// The approved mask per plugin id, mirrored to a file.
pub struct Approvals {
    path: PathBuf,
    granted: Mutex<BTreeMap<String, i64>>,
}

impl Approvals {
    /// Read what has been allowed, or start with nothing allowed.
    ///
    /// A file that cannot be read starts empty, which is the safe direction:
    /// the cost is a prompt the user has answered before, and the
    /// alternative is granting something nobody can account for.
    #[must_use]
    pub fn open(dir: Option<&Path>) -> Self {
        let Some(dir) = dir else {
            return Self {
                path: PathBuf::new(),
                granted: Mutex::new(BTreeMap::new()),
            };
        };
        let path = dir.join("approvals.json");
        let granted = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                log::warn!(
                    "{} is unreadable ({e}); every plugin starts unapproved",
                    path.display()
                );
                BTreeMap::new()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => {
                log::warn!(
                    "cannot read {} ({e}); every plugin starts unapproved",
                    path.display()
                );
                BTreeMap::new()
            }
        };
        Self {
            path,
            granted: Mutex::new(granted),
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
    pub fn set(&self, id: &str, requested: i64, approved: bool) -> i64 {
        let agreed = requested & abi::caps::NEEDS_APPROVAL;
        {
            let mut granted = self.lock();
            if approved {
                granted.insert(id.to_owned(), agreed);
            } else {
                granted.remove(id);
            }
        }
        self.flush();
        self.approved(id)
    }

    /// Write the whole map out, atomically — through a temporary file and a
    /// rename, so a daemon killed mid-write leaves the previous answers
    /// rather than a truncated file that reads as "nothing was allowed".
    fn flush(&self) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        let Ok(json) = serde_json::to_vec(&*self.lock()) else {
            return;
        };
        let temp = self.path.with_extension("json.tmp");
        if let Err(e) =
            std::fs::write(&temp, &json).and_then(|()| std::fs::rename(&temp, &self.path))
        {
            log::warn!(
                "cannot write {}: {e}. Plugin permissions will be asked for again.",
                self.path.display()
            );
            let _ = std::fs::remove_file(&temp);
        }
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
    use oxidezap_plugin_abi as abi;

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
        let a = Approvals::open(Some(&dir.0));
        assert_eq!(a.granted("autoreply", abi::caps::SEND), 0);
        assert!(!a.is_approved("autoreply", abi::caps::SEND));
    }

    /// Drawing, its own settings and its own timer are the plugin's own
    /// business and take effect on declaration — otherwise it could not draw
    /// the panel that explains what it is asking for.
    #[test]
    fn what_a_plugin_does_only_to_itself_needs_no_answer() {
        let dir = TempDir::new("ungated");
        let a = Approvals::open(Some(&dir.0));
        let own = abi::caps::UI | abi::caps::STORAGE | abi::caps::TIMERS;
        assert_eq!(a.granted("p", own), own);
        assert!(a.is_approved("p", own));
    }

    /// And a mixed request keeps the ungated half while the rest waits.
    #[test]
    fn an_unapproved_plugin_keeps_only_what_it_does_to_itself() {
        let dir = TempDir::new("mixed");
        let a = Approvals::open(Some(&dir.0));
        let asked = abi::caps::SEND | abi::caps::UI;
        assert_eq!(a.granted("p", asked), abi::caps::UI);
        assert!(!a.is_approved("p", asked));
    }

    /// And one that asks for nothing needs no prompt: a permission dialog
    /// with nothing in it teaches people to dismiss permission dialogs.
    #[test]
    fn a_plugin_that_asks_for_nothing_is_approved_already() {
        let dir = TempDir::new("nothing");
        let a = Approvals::open(Some(&dir.0));
        assert!(a.is_approved("watcher", 0));
    }

    #[test]
    fn an_answer_survives_a_restart() {
        let dir = TempDir::new("persist");
        let wanted = abi::caps::SEND | abi::caps::UI;
        {
            let a = Approvals::open(Some(&dir.0));
            a.set("autoreply", wanted, true);
        }
        let a = Approvals::open(Some(&dir.0));
        assert_eq!(a.granted("autoreply", wanted), wanted);
        assert!(a.is_approved("autoreply", wanted));
    }

    #[test]
    fn withdrawing_takes_everything_back() {
        let dir = TempDir::new("withdraw");
        let a = Approvals::open(Some(&dir.0));
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
        let a = Approvals::open(Some(&dir.0));
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
        let a = Approvals::open(Some(&dir.0));
        a.set("autoreply", abi::caps::SEND | abi::caps::MARK_READ, true);
        assert_eq!(a.granted("autoreply", abi::caps::SEND), abi::caps::SEND);
        assert!(a.is_approved("autoreply", abi::caps::SEND));
    }

    #[test]
    fn plugins_do_not_share_an_answer() {
        let dir = TempDir::new("separate");
        let a = Approvals::open(Some(&dir.0));
        a.set("one", abi::caps::SEND, true);
        assert_eq!(a.granted("two", abi::caps::SEND), 0);
    }

    #[test]
    fn a_corrupt_file_grants_nothing_rather_than_everything() {
        let dir = TempDir::new("corrupt");
        std::fs::write(dir.0.join("approvals.json"), b"{not json").expect("writable");
        let a = Approvals::open(Some(&dir.0));
        assert_eq!(a.granted("autoreply", abi::caps::SEND), 0);
    }
}
