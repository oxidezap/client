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
    path: PathBuf,
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
        // The same question the approvals file is asked, and for a weaker but
        // real version of the same reason: this directory may have been open
        // before the host closed it, so a file in it can be one another local
        // account wrote. A plugin's settings are not authority — nothing here
        // grants anything — but they *steer* it, and an autoreply reading
        // somebody else's list of phrases is a plugin doing what a stranger
        // configured. Started empty rather than refused, which is what a
        // corrupt file already does.
        if path.exists() && !crate::only_this_user_can_write(&path) {
            log::warn!(
                "{} could have been written by another user on this machine; starting empty",
                path.display()
            );
            let _ = std::fs::remove_file(&path);
            return Self {
                path,
                entries: BTreeMap::new(),
                bytes: 0,
                complained: false,
                wrote_at: None,
                dirty: false,
            };
        }
        // Bounded before it is read, not after it is parsed: the file is this
        // plugin's own and is written under `MAX_BYTES`, so one larger than
        // that is not something this host wrote — and reading it to find out
        // is the allocation the limit exists to refuse.
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES as u64 * 2) {
            log::warn!(
                "{} is larger than a plugin's whole budget; starting empty",
                path.display()
            );
            return Self {
                path,
                entries: BTreeMap::new(),
                bytes: 0,
                complained: false,
                wrote_at: None,
                dirty: false,
            };
        }
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
            bytes: Self::size(&entries),
            entries,
            complained: false,
            wrote_at: None,
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
            bytes: 0,
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

    /// Write the whole map out, atomically.
    ///
    /// Through a temporary file and a rename: a daemon killed mid-write would
    /// otherwise leave a truncated file, which is the one case the "start
    /// empty" recovery above turns into silently losing every setting rather
    /// than the last one.
    /// Whether what is held is now on disk.
    ///
    /// `true` for a memory-only store: there is nothing it could fail to
    /// write, so nothing stays pending.
    fn flush(&mut self) -> bool {
        if self.path.as_os_str().is_empty() {
            return true;
        }
        let Ok(json) = serde_json::to_vec(&self.entries) else {
            return false;
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
        let outcome = crate::write_private(&temp, &json)
            .and_then(|()| std::fs::rename(&temp, &self.path))
            // As the approvals file does it, and for the same reason:
            // syncing the contents does not persist the name. Unlike that
            // file, this one is settings rather than authority — a sync that
            // fails is a preference that may not survive a power cut, so it
            // is reported the same way any other failed write here is, and
            // the value stays dirty to be written again.
            .and_then(|()| match self.path.parent() {
                Some(dir) => crate::sync_dir(dir),
                None => Ok(()),
            });
        if let Err(e) = outcome {
            if !self.complained {
                self.complained = true;
                log::warn!(
                    "cannot write {}: {e}. This plugin's settings will not survive a restart.",
                    self.path.display()
                );
            }
            let _ = std::fs::remove_file(&temp);
            false
        } else {
            self.complained = false;
            true
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
        assert!(kv.set("greeting", "hi"));
        // What the runtime does when the wasm call returns. A `set` alone is
        // not durable, deliberately: writing per key made filesystem I/O
        // something a plugin could ask for without limit.
        kv.commit();
        drop(kv);

        let kv = Kv::open(&dir.0, "autoreply");
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
        let mut kv = Kv::open(&dir.0, "p");

        kv.set("k", "first");
        kv.commit();
        assert!(path.exists(), "the first write is not held back");

        // The next call, immediately: changed in memory, not on disk.
        kv.set("k", "second");
        kv.commit();
        assert_eq!(
            Kv::open(&dir.0, "p").get("k"),
            Some("first"),
            "too soon after the last write, so it stays dirty"
        );

        // And nothing is lost: the plugin stopping writes what is pending.
        kv.flush_pending();
        assert_eq!(Kv::open(&dir.0, "p").get("k"), Some("second"));
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
