//! Where media bytes go when they have to reach another process.
//!
//! A photo is megabytes and the socket carries newline-delimited JSON, so
//! media never travels as a frame: whoever has the bytes writes them here and
//! the other side reads the file. Both sides derive the directory from
//! [`oxidezap_ipc::media_path`], so they cannot disagree about where it is.
//!
//! Keys are content-addressed wherever the content already has an address — a
//! message id, or the encrypted media's SHA-256 — so writing the same payload
//! twice is a no-op and a download the daemon has already served costs
//! nothing the second time. That is the whole cache: no index, no eviction
//! bookkeeping, just files whose names say what is in them.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};

/// How much media the cache may hold before the oldest is dropped.
///
/// Generous, because the alternative to a cache hit is downloading a photo
/// again over the network, and bounded, because nothing else would ever
/// delete these: a session that runs for months would otherwise keep every
/// image it has ever shown.
const CACHE_BUDGET_BYTES: u64 = 512 * 1024 * 1024;

/// Bytes written since the last sweep before another one runs.
///
/// Sweeping reads the whole directory, and a history load writes hundreds of
/// files in a burst; doing it per write would make attaching a front end
/// quadratic in the size of the account.
const SWEEP_INTERVAL_BYTES: u64 = 32 * 1024 * 1024;

/// The key under which a message's own media is cached.
///
/// The message id is already unique and already stable across restarts, so it
/// is the address. Prefixed to keep it from colliding with a download's key,
/// which is addressed by content rather than by message.
///
/// The prefix also says *what* is under it: only full media is cached, so a
/// hit is the real thing. It reads `f-` rather than `m-` because an earlier
/// build wrote fallback thumbnails under the message key too, and a viewer
/// that opened one of those showed a blur at full size. Changing the prefix
/// orphans those files rather than trusting them; the budget sweep clears
/// them in its own time.
pub fn message_key(message_id: &str) -> String {
    format!("f-{}", sanitize(message_id))
}

/// The key under which a downloadable's bytes are cached.
///
/// Its SHA-256 is the encrypted file's identity, so the same media shared into
/// two chats is downloaded once. Truncated: 128 bits is far past collision
/// range for a cache and keeps the name short enough to read.
pub fn download_key(file_enc_sha256: &[u8]) -> String {
    let mut key = String::from("d-");
    for byte in file_enc_sha256.iter().take(16) {
        use std::fmt::Write as _;
        let _ = write!(key, "{byte:02x}");
    }
    key
}

/// Keep a key to the characters [`oxidezap_ipc::media_path`] accepts.
///
/// Message ids come off the network. One carrying a separator would name a
/// file outside the cache, and the daemon writes as the user who owns the
/// session.
fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '.'
            }
        })
        .take(120)
        .collect()
}

/// Write `bytes` under `key`, unless they are already there.
///
/// Returns the key, so a caller can hand it straight to the peer.
pub fn put(key: &str, bytes: &[u8]) -> Result<String> {
    let path = oxidezap_ipc::media_path(key).context("no media cache to write into")?;
    // Content-addressed: the same key is the same bytes, so a file that is
    // already there is already right. Size-checked rather than trusted, so a
    // write cut short by a crash is redone rather than served truncated.
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() == bytes.len() as u64) {
        return Ok(key.to_string());
    }

    let dir = path.parent().context("media path has no parent")?;
    prepare_dir(dir)?;

    // Through a temporary and a rename: a reader that opens the key must
    // never see half a file, and the reader is another process racing this
    // one by design.
    let temp = path.with_extension(format!("part{}", write_ticket()));
    std::fs::write(&temp, bytes).with_context(|| format!("writing {}", temp.display()))?;
    if let Err(e) = std::fs::rename(&temp, &path) {
        // Windows will not rename onto an existing file, and two clients
        // asking for the same uncached media both miss the check above. The
        // other write winning is a cache hit, not a failure — the bytes are
        // content-addressed, so whatever is there is what this was going to
        // put there.
        let _ = std::fs::remove_file(&temp);
        if !std::fs::metadata(&path).is_ok_and(|meta| meta.len() == bytes.len() as u64) {
            return Err(e).with_context(|| format!("renaming into {}", path.display()));
        }
        return Ok(key.to_string());
    }

    sweep_occasionally(dir, bytes.len() as u64);
    Ok(key.to_string())
}

/// Read what is under `key` and remove it.
///
/// For payloads a client staged rather than the daemon cached: their bytes
/// never counted toward the sweep, so nothing else would ever clear them.
pub fn take(key: &str) -> Option<Vec<u8>> {
    let path = oxidezap_ipc::media_path(key)?;
    let bytes = std::fs::read(&path).ok()?;
    if let Err(e) = std::fs::remove_file(&path) {
        log::warn!(
            "could not clear the staged upload at {}: {e}",
            path.display()
        );
    }
    Some(bytes)
}

/// A name no other in-progress write is using.
///
/// The process id alone is not enough: two clients of the same daemon asking
/// for the same uncached media both miss the check above, and would then race
/// on one temporary where a rename can take the file out from under the other
/// write.
fn write_ticket() -> String {
    use portable_atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Whether `key` is already cached, without reading it.
pub fn has(key: &str) -> bool {
    oxidezap_ipc::media_path(key).is_some_and(|path| path.exists())
}

/// The cache directory carries a copy of every photo the account has shown,
/// so it gets the same treatment as the socket beside it: ours alone.
#[cfg(unix)]
fn prepare_dir(dir: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(()),
        // Already there is the common case, and the only failure that is not
        // one: the directory is created once and written to thousands of
        // times.
        Err(_) if dir.is_dir() => Ok(()),
        Err(e) => Err(e).with_context(|| format!("creating {}", dir.display())),
    }
}

#[cfg(not(unix))]
fn prepare_dir(dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))
}

/// Drop the oldest files once enough has been written to be worth looking.
fn sweep_occasionally(dir: &std::path::Path, written: u64) {
    use portable_atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    static SINCE_SWEEP: AtomicU64 = AtomicU64::new(0);

    let before = SINCE_SWEEP.fetch_add(written, Ordering::Relaxed);
    if before + written < SWEEP_INTERVAL_BYTES {
        return;
    }
    SINCE_SWEEP.store(0, Ordering::Relaxed);

    if let Err(e) = sweep(dir) {
        // Not fatal: a cache that is too big is worse than one that is
        // exactly right, and better than a daemon that stopped.
        log::warn!("could not trim the media cache: {e}");
    }
}

/// What the media cache occupies: bytes, and how many files.
///
/// A walk, not a running total: the sweep deletes and the writers add from
/// several tasks, and a counter kept alongside them would be one more thing to
/// keep true. The directory holds a few hundred flat files at most, so asking
/// it is cheap enough to do when a person opens the Storage pane.
pub fn cache_usage() -> (u64, u64) {
    let Some(dir) = oxidezap_ipc::media_dir() else {
        return (0, 0);
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return (0, 0);
    };
    entries
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| meta.is_file())
        .fold((0, 0), |(bytes, files), meta| {
            (bytes + meta.len(), files + 1)
        })
}

/// How much of the directory a wipe is entitled to.
///
/// The directory holds two different things under one roof. `f-` and `d-` are
/// the cache: bytes the daemon fetched, which it can always fetch again.
/// `u-` is not — it is a payload a front end staged for a send that has not
/// run yet, and the only copy of it. Deleting one turns an unrelated "clear
/// cached media" into a voice note that fails with "no audio cached".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wipe {
    /// Cached downloads only.
    Cache,
    /// Everything, staged uploads included: the account is going, and so is
    /// anything that was going to be sent under it.
    Everything,
}

impl Wipe {
    /// Whether a file named `name` is this wipe's to take.
    fn takes(self, name: &str) -> bool {
        match self {
            Self::Everything => true,
            Self::Cache => name.starts_with("f-") || name.starts_with("d-"),
        }
    }
}

/// Which cache the writers still in flight think they are writing into.
///
/// A download dispatched before a wipe finishes after it, and the eager cache
/// of an inbound message can be queued across one. Neither can be cancelled,
/// so the answer is the same as everywhere else in this codebase: bump a
/// number and let the writer notice.
static CACHE_EPOCH: AtomicUsize = AtomicUsize::new(0);

/// What to hand back to [`put_since`] later.
pub fn epoch() -> usize {
    CACHE_EPOCH.load(Ordering::SeqCst)
}

/// Cache `bytes` unless the cache has been cleared since `epoch`.
///
/// For writes nobody is waiting on — the eager cache of an inbound message,
/// which the front end can always fetch on demand instead. A download somebody
/// *asked* for uses [`put`]: the file is how those bytes are delivered, not
/// merely where they are remembered, so refusing it would fail the download
/// rather than keep the directory tidy.
pub fn put_since(epoch: usize, key: &str, bytes: &[u8]) -> Result<String> {
    if CACHE_EPOCH.load(Ordering::SeqCst) != epoch {
        anyhow::bail!("the media cache was cleared while this was being prepared");
    }
    put(key, bytes)
}

/// Delete the cached files this wipe is entitled to.
///
/// Part of "clear data and pair again", and not optional there: the store is
/// one file, but the media beside it is a directory that can hold half a
/// gigabyte of the *previous* account's photos, videos and documents. Leaving
/// it in place means pairing a different account onto a cache of someone
/// else's pictures, with no control anywhere that clears them.
///
/// Best-effort per entry: one unreadable file must not abandon the rest.
pub fn wipe(scope: Wipe) -> Result<()> {
    // Before the deletions, not after: a writer that reads the epoch between
    // the two would otherwise believe its file survived the wipe that is
    // about to remove it.
    CACHE_EPOCH.fetch_add(1, Ordering::SeqCst);
    let Some(dir) = oxidezap_ipc::media_dir() else {
        return Ok(());
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // Never downloaded anything, so there is nothing to clear.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let mut removed = 0usize;
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        if !scope.takes(&entry.file_name().to_string_lossy()) {
            continue;
        }
        if std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    log::info!(
        "cleared {removed} media files ({scope:?}) from {}",
        dir.display()
    );
    Ok(())
}

/// Delete oldest-first until the cache is under budget.
fn sweep(dir: &std::path::Path) -> Result<()> {
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    let mut total = 0u64;
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let age = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        total = total.saturating_add(meta.len());
        files.push((age, meta.len(), entry.path()));
    }
    if total <= CACHE_BUDGET_BYTES {
        return Ok(());
    }

    files.sort_by_key(|(age, ..)| *age);
    for (_, len, path) in files {
        if total <= CACHE_BUDGET_BYTES {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A "clear cached media" that takes a staged upload with it turns an
    /// unrelated cleanup into a voice note that fails with "no audio cached".
    #[test]
    fn clearing_the_cache_spares_a_staged_upload() {
        assert!(Wipe::Cache.takes("f-3EB0ABC"));
        assert!(Wipe::Cache.takes("d-9f86d081884c7d65"));
        assert!(
            !Wipe::Cache.takes("u-local_audio-7"),
            "somebody is still waiting for that to be sent"
        );
    }

    /// The account is going, and so is anything that was going to be sent
    /// under it.
    #[test]
    fn forgetting_an_account_takes_everything() {
        assert!(Wipe::Everything.takes("f-3EB0ABC"));
        assert!(Wipe::Everything.takes("d-9f86d081884c7d65"));
        assert!(Wipe::Everything.takes("u-local_audio-7"));
    }

    /// A message id comes off the network. One carrying a separator would name
    /// a file outside the cache, which the daemon writes to as the user who
    /// owns the session.
    #[test]
    fn a_key_built_from_a_message_id_cannot_escape_the_cache() {
        for id in ["../../etc/passwd", "a/b", "..", "with space", "\0"] {
            let key = message_key(id);
            assert!(
                oxidezap_ipc::media_path(&key).is_some(),
                "{id} produced an unusable key: {key}"
            );
            assert!(!key.contains('/'), "{key}");
        }
    }

    /// The same media shared into two chats is one file, so the second
    /// download never happens.
    #[test]
    fn the_same_content_is_the_same_key() {
        let sha = [0xab_u8; 32];
        assert_eq!(download_key(&sha), download_key(&sha));
        assert_ne!(download_key(&sha), download_key(&[0xcd; 32]));
        assert!(
            download_key(&sha).len() < 40,
            "a key is a file name a person may have to read"
        );
    }

    /// Message keys and download keys share a directory and must not collide:
    /// one is addressed by message, the other by content. The message prefix
    /// is `f-` because it also promises *full* media — see [`message_key`].
    #[test]
    fn the_two_key_spaces_stay_apart() {
        assert!(message_key("abc").starts_with("f-"));
        assert!(download_key(&[1; 32]).starts_with("d-"));
        assert!(
            !message_key("abc").starts_with("m-"),
            "the old prefix cached thumbnails under it and must stay orphaned"
        );
    }
}
