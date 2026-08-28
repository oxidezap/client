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
use std::path::{Path, PathBuf};

/// How much one plugin may keep.
///
/// A settings panel and a set of ids it has already answered, not a database.
/// A plugin that wants more than this wants something the daemon should be
/// asked for instead.
const MAX_BYTES: usize = 256 * 1024;

/// The longest key or value.
const MAX_ENTRY: usize = 8 * 1024;

/// One plugin's key-value pairs, mirrored to a file.
///
/// Held in memory and written through on every change. A plugin's whole store
/// is kilobytes and a write happens when a person flips a toggle, so batching
/// would buy nothing and would open the window where a daemon that stops
/// loses the setting someone just changed.
pub struct Kv {
    path: PathBuf,
    entries: BTreeMap<String, String>,
    /// Set once a write has failed, so a full disk is reported once rather
    /// than on every key a plugin touches afterwards.
    complained: bool,
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
    /// A file that cannot be read is logged and treated as empty rather than
    /// refused: a plugin whose settings file was corrupted should come up
    /// with its defaults, not fail to load. The first write replaces it.
    #[must_use]
    pub fn open(dir: &Path, id: &str) -> Self {
        // Prefixed, so no plugin id can name a file the host keeps in the
        // same directory. A plugin called `approvals` would otherwise write
        // its own settings over `approvals.json` every time somebody changed
        // one — every permission answer on the machine unreadable, and read
        // back on the next start as "nothing was allowed".
        let path = dir.join(format!("kv-{id}.json"));
        let entries = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                log::warn!("plugin {id}: its stored settings are unreadable ({e}); starting empty");
                BTreeMap::new()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => {
                log::warn!("plugin {id}: cannot read its settings ({e}); starting empty");
                BTreeMap::new()
            }
        };
        Self {
            path,
            entries,
            complained: false,
            dirty: false,
        }
    }

    /// A store with nowhere to write, for a host that has no state directory.
    ///
    /// Reads and writes work for the life of the process and nothing
    /// survives it. Better than refusing to run the plugin: a machine with no
    /// writable home is one where the account itself is already in trouble.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            path: PathBuf::new(),
            entries: BTreeMap::new(),
            complained: false,
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
            if self.entries.remove(key).is_some() {
                self.dirty = true;
            }
            return true;
        }
        // Measured against what the store would become, not what it is: a
        // plugin at the limit must not be able to grow one more key past it.
        let replacing = self.entries.get(key).map_or(0, |v| v.len() + key.len());
        let after = self.size().saturating_sub(replacing) + key.len() + value.len();
        if after > MAX_BYTES {
            return false;
        }
        if self.entries.get(key).is_some_and(|v| v == value) {
            // Writing a value that is already there would rewrite the file
            // for nothing, and a toggle redrawn on every event does exactly
            // that.
            return true;
        }
        self.entries.insert(key.to_owned(), value.to_owned());
        self.dirty = true;
        true
    }

    /// Write out whatever the call changed, if it changed anything.
    ///
    /// Called once when a wasm call returns, which is what bounds the I/O a
    /// plugin can ask for: it may set a key a million times and still cost
    /// one file.
    pub fn commit(&mut self) {
        if std::mem::take(&mut self.dirty) {
            self.flush();
        }
    }

    fn size(&self) -> usize {
        self.entries.iter().map(|(k, v)| k.len() + v.len()).sum()
    }

    /// Write the whole map out, atomically.
    ///
    /// Through a temporary file and a rename: a daemon killed mid-write would
    /// otherwise leave a truncated file, which is the one case the "start
    /// empty" recovery above turns into silently losing every setting rather
    /// than the last one.
    fn flush(&mut self) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        let Ok(json) = serde_json::to_vec(&self.entries) else {
            return;
        };
        // Unique per process and thread, like the approvals file's. A fixed
        // name is one two daemons sharing a state directory both write, so
        // one can rename a file the other is still filling — and a plugin's
        // settings then read back as corrupt and start empty.
        let temp = self.path.with_extension(format!(
            "json.{}.{:?}.tmp",
            std::process::id(),
            std::thread::current().id()
        ));
        let outcome =
            crate::write_private(&temp, &json).and_then(|()| std::fs::rename(&temp, &self.path));
        if let Err(e) = outcome {
            if !self.complained {
                self.complained = true;
                log::warn!(
                    "cannot write {}: {e}. This plugin's settings will not survive a restart.",
                    self.path.display()
                );
            }
            let _ = std::fs::remove_file(&temp);
        } else {
            self.complained = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut kv = Kv::open(&dir.0, "autoreply");
        assert!(kv.set("greeting", "oi"));
        // What the runtime does when the wasm call returns. A `set` alone is
        // not durable, deliberately: writing per key made filesystem I/O
        // something a plugin could ask for without limit.
        kv.commit();
        drop(kv);

        let kv = Kv::open(&dir.0, "autoreply");
        assert_eq!(kv.get("greeting"), Some("oi"));
    }

    #[test]
    fn an_empty_value_deletes() {
        let dir = TempDir::new("delete");
        let mut kv = Kv::open(&dir.0, "p");
        kv.set("k", "v");
        assert!(kv.set("k", ""));
        assert_eq!(kv.get("k"), None);
    }

    #[test]
    fn plugins_do_not_share_a_file() {
        let dir = TempDir::new("separate");
        let mut a = Kv::open(&dir.0, "a");
        a.set("k", "from a");
        a.commit();
        let b = Kv::open(&dir.0, "b");
        assert_eq!(b.get("k"), None);
    }

    #[test]
    fn a_corrupt_file_starts_empty_rather_than_failing() {
        let dir = TempDir::new("corrupt");
        // The name `Kv::open` actually reads. Writing `p.json` exercised the
        // missing-file branch instead, so a regression making a corrupt
        // settings file fatal would have passed.
        std::fs::write(dir.0.join("kv-p.json"), b"{not json").expect("writable");
        let mut kv = Kv::open(&dir.0, "p");
        assert_eq!(kv.get("anything"), None);
        assert!(kv.set("k", "v"), "and it is usable again");
    }

    #[test]
    fn a_plugin_cannot_grow_past_its_budget() {
        let dir = TempDir::new("budget");
        let mut kv = Kv::open(&dir.0, "p");
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
        let mut kv = Kv::open(&dir.0, "p");
        let big = "y".repeat(MAX_ENTRY);
        while kv.set(&format!("k{}", kv.entries.len()), &big) {}
        assert!(kv.set("k0", &big), "same size, same key");
    }

    #[test]
    fn an_oversized_entry_is_refused_outright() {
        let dir = TempDir::new("oversized");
        let mut kv = Kv::open(&dir.0, "p");
        assert!(!kv.set("k", &"z".repeat(MAX_ENTRY + 1)));
        assert!(!kv.set("", "v"));
    }

    #[test]
    fn a_store_with_nowhere_to_write_still_works() {
        let mut kv = Kv::in_memory();
        assert!(kv.set("k", "v"));
        assert_eq!(kv.get("k"), Some("v"));
    }
}
