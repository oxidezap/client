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
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};

use super::{CACHE_EPOCH, IN_PROGRESS_PREFIX, WIPE_LOCK, Wipe, is_in_progress, is_staged_upload};
#[cfg(test)]
use super::{download_key, message_key};

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

/// Claim `key` for a delivery, if it is here.
///
/// The same question as [`has`] on this side, and answered the same way,
/// because the front end this cache was built for opens the file itself:
/// there is nothing between promising it and handing it over for anything to
/// close. The distinction is the page's, where the cache is a map somebody
/// else is sweeping.
///
/// A browser attached to *this* daemon is the case that does not fit, and it
/// is worth naming rather than leaving the sentence above to imply it away:
/// there the bytes cross as HTTP, so the promise and the read are two round
/// trips with a gap between them. What can delete a file in that gap is a
/// `ClearMediaCache` and not the budget sweep — the sweep drops the oldest,
/// and a key just promised was just written — so the window is somebody
/// pressing "clear cached media" in the same millisecond as their own
/// download. What it costs is one refetch: the renderer draws media it does
/// not have as an offer to download, which is what it already does for
/// anything the daemon never cached. Closing it means giving this cache the
/// index its first line says it does not have; see AGENTS.md.
pub fn claim(key: &str) -> bool {
    has(key)
}

/// The same as [`put`], for a caller that already owns the only copy.
///
/// Nothing to save here — the bytes are written to a file either way — so
/// this simply forwards. It exists because on the page both copies live in
/// one linear memory with a ceiling, and a large download can be a
/// meaningful fraction of it.
pub fn put_owned(key: &str, bytes: Vec<u8>) -> Result<String> {
    put(key, &bytes)
}

/// Cache `bytes` under `key`, droppable from the moment they land.
///
/// The same call as [`put`] here. The distinction exists for the page, whose
/// cache is its own heap and whose sweep therefore has to know which entries
/// are somebody's pending answer; this side writes a file, and the budget
/// sweep runs on its own schedule against a disk that is two orders of
/// magnitude larger.
pub fn put_evictable(key: &str, bytes: &[u8]) -> Result<String> {
    put(key, bytes)
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
    // Under a name nothing else claims: a temporary called `<key>.partN`
    // carried the key's own prefix, so a wipe and the budget sweep both read
    // a download in flight as theirs to delete.
    let temp = dir.join(format!("{IN_PROGRESS_PREFIX}{}", write_ticket()));
    std::fs::write(&temp, bytes).with_context(|| format!("writing {}", temp.display()))?;
    // The rename under the wipe lock, so the file is either wholly before a
    // clear — and taken by it — or wholly after. What it cannot cover is the
    // caller's answer, which is a round trip later: a clear landing there
    // costs one refetch, since media the renderer does not have is drawn as
    // an offer to download. See `claim`.
    let renamed = {
        let _guard = WIPE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::fs::rename(&temp, &path)
    };
    if let Err(e) = renamed {
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

/// Best-effort per entry: one unreadable file must not abandon the rest.
pub fn wipe(scope: Wipe) -> Result<()> {
    // For the whole wipe, so an epoch-checked write is either wholly before
    // it — and deleted by it — or wholly after, and kept.
    let _guard = WIPE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        // Everything but a staged upload is evictable, which is more than
        // `Wipe::Cache` names: the `m-` files an older build left behind are
        // orphans with nothing else to clear them — `message_key` says as much
        // — and `cache_usage` bills the directory by every file in it, so the
        // budget has to be enforced over the same set. A staged upload is the
        // exception because it is the only copy of a voice note somebody is
        // waiting to have sent, and it never counted toward the budget it
        // would be dropped for: `put` is what feeds the sweep, and an upload
        // is written by the front end, not through it.
        // A write in progress is nobody's to reclaim either: the bytes are
        // not there yet, and whoever asked for them is waiting.
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_staged_upload(&name) || is_in_progress(&name) {
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

    /// A download in flight is written under a name of its own. Called
    /// `<key>.partN` it carried the key's prefix, so a clear pressed in the
    /// same moment deleted the file somebody was waiting for.
    #[test]
    fn a_write_in_progress_is_nobodys_to_delete() {
        let temp = format!("{IN_PROGRESS_PREFIX}1234.0");
        assert!(is_in_progress(&temp));
        assert!(!Wipe::Cache.takes(&temp));
        assert!(!is_staged_upload(&temp));
        // The old spelling, which both of them claimed.
        assert!(Wipe::Cache.takes("d-9f86d081884c7d65.part1234.0"));
        // The account leaving takes everything, temporaries included; the
        // epoch is what stops the writer putting it back.
        assert!(Wipe::Everything.takes(&temp));
    }

    /// The budget sweep reclaims more than a "clear cached media" does: the
    /// orphans of an older build have nothing else to remove them, and they
    /// are billed to the user by `cache_usage` either way.
    #[test]
    fn the_budget_sweep_spares_only_a_staged_upload() {
        assert!(is_staged_upload("u-local_audio-7"));
        assert!(!is_staged_upload("f-3EB0ABC"));
        assert!(!is_staged_upload("d-9f86d081884c7d65"));
        assert!(
            !is_staged_upload("m-3EB0ABC"),
            "an orphan of the build that wrote thumbnails under the message key"
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
