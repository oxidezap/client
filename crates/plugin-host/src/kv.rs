//! A plugin's own small store.
//!
//! Deliberately not the chat store. That file is one file — device identity,
//! Signal state and history keyed by device id — and giving a plugin a table
//! in it would put a schema the store does not understand behind the same
//! migrations, in the same wipe. A JSON file per plugin costs nothing to
//! reason about, and losing one loses a plugin's settings rather than an
//! account.
//!
//! It is still the *account's* data: an autoreply's "already answered these
//! people" is a list of people. So the directory sits beside the store and
//! goes when the account goes.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::store::Backing;

/// How much one plugin may keep.
///
/// A settings panel and a set of ids it has already answered, not a database.
/// A plugin that wants more than this wants something the daemon should be
/// asked for instead.
const MAX_BYTES: usize = 256 * 1024;

/// The longest key or value.
pub(crate) const MAX_ENTRY: usize = 8 * 1024;

/// The shortest time between two writes of this file.
///
/// One write per call bounds what *one* handler costs, and a plugin gives
/// itself handlers: sixteen timers at the hundred-millisecond floor, each
/// changing one byte, is a hundred and sixty serializations, `fsync`s,
/// renames and directory flushes a second — per plugin, for a store nobody
/// asked to keep. So a commit that comes too soon leaves the change dirty
/// and the next one writes it; nothing is lost, because dirty is exactly
/// what "not on disk yet" means everywhere else in this file.
///
/// A second, because that is the scale of the thing being protected: a
/// person flipping a toggle waits nothing, and a plugin looping waits.
const MIN_WRITE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// One plugin's key-value pairs, mirrored to a file.
///
/// Held in memory and written through on every change. A plugin's whole store
/// is kilobytes and a write happens when a person flips a toggle, so batching
/// would buy nothing and would open the window where a daemon that stops
/// loses the setting someone just changed.
pub struct Kv {
    store: Arc<dyn Backing>,
    /// What this plugin's document is called. Prefixed with `kv-`, which is
    /// what keeps a plugin id from naming the approvals document.
    name: String,
    entries: BTreeMap<String, String>,
    /// Set once a write has failed, so a full disk is reported once rather
    /// than on every key a plugin touches afterwards.
    complained: bool,
    /// What the entries weigh, kept as a running total. See [`Kv::size`].
    bytes: usize,
    /// When this file was last written, so a plugin that changes a key on
    /// every callback does not turn its own timer into disk I/O. See
    /// [`MIN_WRITE_INTERVAL`].
    wrote_at: Option<wacore::time::Instant>,
    /// Whether anything has changed since the last write.
    ///
    /// A `set` no longer writes; [`commit`](Self::commit) does, once, after
    /// the wasm call returns. Writing per key made filesystem I/O something a
    /// plugin could ask for without limit — fuel does not price a rename, and
    /// `oxi_init` has two hundred million of it — so a small downloaded
    /// module alternating one key could stall the daemon's whole startup.
    /// Folding them also makes the ordinary case cheaper: a plugin that
    /// stores three settings from one press writes one file, not three.
    dirty: bool,
}

impl Kv {
    /// Open the store for one plugin, or start empty.
    ///
    /// A document that cannot be read is logged and treated as empty rather
    /// than refused: a plugin whose settings were corrupted should come up
    /// with its defaults, not fail to load. The first write replaces it.
    #[must_use]
    pub fn open(store: Arc<dyn Backing>, id: &str) -> Self {
        // Prefixed, so no plugin id can name a document the host keeps in the
        // same place. A plugin called `approvals` would otherwise write its
        // own settings over `approvals.json` every time somebody changed one
        // — every permission answer unreadable, and read back on the next
        // start as "nothing was allowed".
        let name = format!("kv-{id}.json");
        // The ceiling handed to the store is on the *encoded* size, which is
        // not the budget: `MAX_BYTES` counts the bytes a plugin stored, and
        // JSON writes a control character as `\u0000` — six bytes for one. A
        // store full of them is a valid store this host wrote and would
        // serialize past twice the budget, so a ceiling of `MAX_BYTES * 2`
        // refused the host's own document and started the plugin empty,
        // losing every setting it had. Eight covers the six with room for
        // the quotes and commas around it, and what it decodes to is checked
        // against the real budget below.
        let entries: BTreeMap<String, String> = store
            .read(&name, MAX_BYTES * 8)
            .and_then(|bytes| {
                serde_json::from_slice(&bytes)
                    .map_err(|e| {
                        log::warn!(
                            "plugin {id}: its stored settings are unreadable ({e}); starting empty"
                        );
                    })
                    .ok()
            })
            .unwrap_or_default();
        // And what it decoded to, against the budget itself. The ceiling
        // above is about what may be *read*; this is the one that says
        // whether the contents are something this host would have written.
        let entries = if Self::size(&entries) > MAX_BYTES {
            log::warn!(
                "plugin {id}: its stored settings hold more than its whole budget; starting empty"
            );
            BTreeMap::new()
        } else {
            entries
        };
        Self {
            store,
            name,
            bytes: Self::size(&entries),
            entries,
            complained: false,
            wrote_at: None,
            dirty: false,
        }
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    /// Store a value, or remove it when the value is empty.
    ///
    /// Empty means delete because the ABI has no third call for it, and by
    /// the absence rule an empty string and a missing one read back the same
    /// anyway — so a separate `oxi_kv_del` would be an import that could not
    /// express anything `oxi_kv_set` cannot.
    ///
    /// Returns whether it was stored. A refusal is a plugin past its budget,
    /// which it is told about rather than left to discover by reading back a
    /// value that is not there.
    pub fn set(&mut self, key: &str, value: &str) -> bool {
        if key.is_empty() || key.len() > MAX_ENTRY || value.len() > MAX_ENTRY {
            return false;
        }
        if value.is_empty() {
            if let Some(old) = self.entries.remove(key) {
                self.bytes = self.bytes.saturating_sub(old.len() + key.len());
                self.dirty = true;
            }
            return true;
        }
        // Asked first, and cheaply: writing a value that is already there
        // would rewrite the file for nothing, and a toggle redrawn on every
        // event does exactly that. Before the budget, because the budget used
        // to walk the whole map — so a plugin storing the same value in a
        // loop paid a full scan per call, which for a store of thousands of
        // small keys is host work no fuel accounts for.
        if self.entries.get(key).is_some_and(|v| v == value) {
            return true;
        }
        // Measured against what the store would become, not what it is: a
        // plugin at the limit must not be able to grow one more key past it.
        // Kept incrementally rather than recomputed, for the same reason.
        let replacing = self.entries.get(key).map_or(0, |v| v.len() + key.len());
        let after = self.bytes.saturating_sub(replacing) + key.len() + value.len();
        if after > MAX_BYTES {
            return false;
        }
        self.entries.insert(key.to_owned(), value.to_owned());
        self.bytes = after;
        self.dirty = true;
        true
    }

    /// Write out whatever the call changed, if it changed anything.
    ///
    /// Called once when a wasm call returns, which is what bounds the I/O a
    /// plugin can ask for: it may set a key a million times and still cost
    /// one file.
    pub fn commit(&mut self) {
        // Too soon since the last one is not "never": the change stays dirty
        // and the next commit — the next event, the next timer, or the flush
        // when this plugin stops — writes it.
        if self
            .wrote_at
            .is_some_and(|at| at.elapsed() < MIN_WRITE_INTERVAL)
        {
            return;
        }
        self.write_out();
    }

    /// When the pending change may be written, if there is one.
    ///
    /// The worker asks before it goes to sleep. A commit held back for the
    /// interval used to wait for the *next* call, and a plugin that changes a
    /// setting and then hears nothing again has no next call — so the change
    /// sat in memory for as long as the plugin was quiet, which is exactly
    /// the case a person flipping one toggle produces.
    #[must_use]
    pub fn due_at(&self) -> Option<wacore::time::Instant> {
        if !self.dirty {
            return None;
        }
        Some(match self.wrote_at {
            Some(at) => at + MIN_WRITE_INTERVAL,
            None => wacore::time::Instant::now(),
        })
    }

    /// Write what is pending, whatever the interval says.
    ///
    /// For the one place where there is no next commit: a plugin that is
    /// stopping. Everything else goes through [`commit`](Self::commit).
    pub fn flush_pending(&mut self) {
        self.write_out();
    }

    fn write_out(&mut self) {
        if self.dirty {
            // Cleared only by a write that landed. Taking the flag first
            // meant a full disk lost the change silently: the map held it,
            // nothing was dirty any more, and the next restart read back the
            // older file.
            self.dirty = !self.flush();
            self.wrote_at = Some(wacore::time::Instant::now());
        }
    }

    /// What the entries weigh, computed once when the store is opened.
    ///
    /// Kept as a running total afterwards. Recomputing it per write was a
    /// walk of the whole map on a path a plugin controls: the budget allows
    /// tens of thousands of small keys, so a short loop turned a handful of
    /// fixed-price imports into billions of host-side iterations.
    fn size(entries: &BTreeMap<String, String>) -> usize {
        entries.iter().map(|(k, v)| k.len() + v.len()).sum()
    }

    /// Write the whole map out.
    ///
    /// Answers whether what is held is now stored. `true` for a memory-only
    /// store: there is nothing it could fail to write, so nothing stays
    /// pending.
    fn flush(&mut self) -> bool {
        let Ok(json) = serde_json::to_vec(&self.entries) else {
            return false;
        };
        match self.store.write(&self.name, &json) {
            Ok(()) => {
                self.complained = false;
                true
            }
            Err(e) => {
                if !self.complained {
                    self.complained = true;
                    log::warn!(
                        "cannot write {}: {e}. This plugin's settings will not survive a \
                         restart.",
                        self.store.describe(&self.name)
                    );
                }
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Files;
    use std::path::{Path, PathBuf};

    /// The store these tests are about: files in a directory of their own.
    fn files(dir: &Path) -> Arc<dyn Backing> {
        Arc::new(Files::at(dir))
    }

    /// A store this host wrote has to be a store it can read back.
    ///
    /// JSON writes a control character as six bytes, so a plugin can fill its
    /// budget with values that serialize to far more than the budget — and a
    /// ceiling set at twice it then refused the daemon's own file and started
    /// the plugin empty, losing every setting it had.
    #[test]
    fn a_store_full_of_escapes_survives_a_restart() {
        let dir = TempDir::new("escapes");
        let mut kv = Kv::open(files(&dir.0), "escaper");

        // Every byte of this value is written as `\u0000`: six on disk for
        // one in the budget.
        let nuls = "\0".repeat(MAX_ENTRY);
        for i in 0..16 {
            assert!(kv.set(&format!("k{i}"), &nuls), "within the budget");
        }
        kv.flush_pending();

        let reopened = Kv::open(files(&dir.0), "escaper");
        assert_eq!(
            reopened.get("k0"),
            Some(nuls.as_str()),
            "what it wrote is what it reads"
        );
        assert_eq!(reopened.get("k15"), Some(nuls.as_str()));
    }

    /// The other half: a file whose *contents* are past the budget is not
    /// something this host wrote, whatever its encoded size.
    #[test]
    fn settings_holding_more_than_the_budget_are_not_read() {
        let dir = TempDir::new("oversized");
        let path = dir.0.join("kv-fat.json");
        let mut entries = std::collections::BTreeMap::new();
        for i in 0..64 {
            entries.insert(format!("k{i}"), "v".repeat(MAX_ENTRY));
        }
        std::fs::write(&path, serde_json::to_vec(&entries).expect("encodes")).expect("writable");

        let kv = Kv::open(files(&dir.0), "fat");
        assert_eq!(kv.get("k0"), None, "started empty");
    }

    /// A directory that removes itself, so these tests leave nothing behind
    /// and do not need a crate for it.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "oxidezap-kv-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&path).expect("a writable temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_value_survives_reopening() {
        let dir = TempDir::new("reopen");
        let mut kv = Kv::open(files(&dir.0), "autoreply");
        assert!(kv.set("greeting", "hi"));
        // What the runtime does when the wasm call returns. A `set` alone is
        // not durable, deliberately: writing per key made filesystem I/O
        // something a plugin could ask for without limit.
        kv.commit();
        drop(kv);

        let kv = Kv::open(files(&dir.0), "autoreply");
        assert_eq!(kv.get("greeting"), Some("hi"));
    }

    /// One write per call bounds one handler, and a plugin gives itself
    /// handlers: sixteen timers at the floor, each changing a byte, is a
    /// hundred and sixty serializations, `fsync`s and renames a second. A
    /// commit that comes too soon leaves the change dirty for the next one.
    #[test]
    fn a_plugin_cannot_turn_its_own_timer_into_disk_writes() {
        let dir = TempDir::new("debounce");
        let path = dir.0.join("kv-p.json");
        let mut kv = Kv::open(files(&dir.0), "p");

        kv.set("k", "first");
        kv.commit();
        assert!(path.exists(), "the first write is not held back");

        // The next call, immediately: changed in memory, not on disk.
        kv.set("k", "second");
        kv.commit();
        assert_eq!(
            Kv::open(files(&dir.0), "p").get("k"),
            Some("first"),
            "too soon after the last write, so it stays dirty"
        );

        // And nothing is lost: the plugin stopping writes what is pending.
        kv.flush_pending();
        assert_eq!(Kv::open(files(&dir.0), "p").get("k"), Some("second"));
    }

    #[test]
    fn an_empty_value_deletes() {
        let dir = TempDir::new("delete");
        let mut kv = Kv::open(files(&dir.0), "p");
        kv.set("k", "v");
        assert!(kv.set("k", ""));
        assert_eq!(kv.get("k"), None);
    }

    #[test]
    fn plugins_do_not_share_a_file() {
        let dir = TempDir::new("separate");
        let mut a = Kv::open(files(&dir.0), "a");
        a.set("k", "from a");
        a.commit();
        let b = Kv::open(files(&dir.0), "b");
        assert_eq!(b.get("k"), None);
    }

    #[test]
    fn a_corrupt_file_starts_empty_rather_than_failing() {
        let dir = TempDir::new("corrupt");
        // The name `Kv::open` actually reads. Writing `p.json` exercised the
        // missing-file branch instead, so a regression making a corrupt
        // settings file fatal would have passed.
        std::fs::write(dir.0.join("kv-p.json"), b"{not json").expect("writable");
        let mut kv = Kv::open(files(&dir.0), "p");
        assert_eq!(kv.get("anything"), None);
        assert!(kv.set("k", "v"), "and it is usable again");
    }

    #[test]
    fn a_plugin_cannot_grow_past_its_budget() {
        let dir = TempDir::new("budget");
        let mut kv = Kv::open(files(&dir.0), "p");
        let chunk = "x".repeat(MAX_ENTRY);
        let mut stored = 0;
        for i in 0..64 {
            if kv.set(&format!("k{i}"), &chunk) {
                stored += 1;
            }
        }
        assert!(stored > 0, "some fit");
        assert!(stored < 64, "and it stopped before the budget was gone");
    }

    /// Replacing a key at the limit must still work: the check is against
    /// what the store *becomes*, not against what it holds.
    #[test]
    fn replacing_a_value_is_not_growing() {
        let dir = TempDir::new("replace");
        let mut kv = Kv::open(files(&dir.0), "p");
        let big = "y".repeat(MAX_ENTRY);
        while kv.set(&format!("k{}", kv.entries.len()), &big) {}
        assert!(kv.set("k0", &big), "same size, same key");
    }

    #[test]
    fn an_oversized_entry_is_refused_outright() {
        let dir = TempDir::new("oversized");
        let mut kv = Kv::open(files(&dir.0), "p");
        assert!(!kv.set("k", &"z".repeat(MAX_ENTRY + 1)));
        assert!(!kv.set("", "v"));
    }

    #[test]
    fn a_store_with_nowhere_to_write_still_works() {
        let mut kv = Kv::open(Arc::new(crate::store::Nowhere), "p");
        assert!(kv.set("k", "v"));
        assert_eq!(kv.get("k"), Some("v"));
    }
}
