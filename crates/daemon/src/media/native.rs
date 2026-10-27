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

use anyhow::{Context, Result};

use super::{IN_PROGRESS_PREFIX, Wipe, is_disposable, is_in_progress, is_staging_partial};

/// How much media the cache may hold before the oldest is dropped.
///
/// Generous, because the alternative to a cache hit is downloading a photo
/// again over the network, and bounded, because nothing else would ever
/// delete these: a session that runs for months would otherwise keep every
/// image it has ever shown.
const CACHE_BUDGET_BYTES: u64 = 512 * 1024 * 1024;

/// Grace for incomplete uploads whose writer is no longer active. Completed
/// staged payloads are retained until consumed or explicitly discarded.
const STALE_PARTIAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// Bytes written since the last sweep before another one runs.
///
/// Sweeping reads the whole directory, and a history load writes hundreds of
/// files in a burst; doing it per write would make attaching a front end
/// quadratic in the size of the account.
const SWEEP_INTERVAL_BYTES: u64 = 32 * 1024 * 1024;
const MAX_CACHE_FILES: usize = 8192;
const SWEEP_INTERVAL_WRITES: u64 = 256;

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
/// index its first line says it does not have; see docs/roadmap.md.
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

pub fn put_owned_evictable(key: &str, bytes: Vec<u8>) -> Result<String> {
    put_owned(key, bytes)
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
    let dir = path.parent().context("media path has no parent")?;
    // Before the hit below and not after it. `metadata` follows a symlink, and
    // a key here is derived from content the account has already published, so
    // a link of the right length planted under one while the directory was
    // open answered "already cached" and the daemon then served its target as
    // the account's own media -- returning before the sweep that exists to
    // remove it had run at all.
    prepare_dir(dir)?;

    // Content-addressed: the same key is the same bytes, so a file that is
    // already there is already right. Size-checked rather than trusted, so a
    // write cut short by a crash is redone rather than served truncated.
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() == bytes.len() as u64) {
        return Ok(key.to_string());
    }

    // Through a temporary and a rename: a reader that opens the key must
    // never see half a file, and the reader is another process racing this
    // one by design.
    // Under a name nothing else claims: a temporary called `<key>.partN`
    // carried the key's own prefix, so a wipe and the budget sweep both read
    // a download in flight as theirs to delete.
    //
    // `create_new`, so nothing already at this name is opened: a plain
    // `write` follows a symlink to wherever it points and truncates whatever
    // is there. A leftover from a process that shared this pid and sequence
    // and died mid-write is the one honest way to meet one, so it is unlinked
    // once — the link and not its target — and the create tried again.
    let temp = dir.join(format!("{IN_PROGRESS_PREFIX}{}", write_ticket()));
    let _writing = track_write(temp.clone());
    write_new(&temp, bytes).with_context(|| format!("writing {}", temp.display()))?;
    // The rename happens under the wipe lock, which every entry point in
    // `media` takes before calling here: the file is either wholly before a
    // clear — and taken by it — or wholly after. What that cannot cover is
    // the caller's answer, which is a round trip later; a clear landing
    // there costs one refetch, since media the renderer does not have is
    // drawn as an offer to download. See `claim`.
    if let Err(e) = std::fs::rename(&temp, &path) {
        // A competing writer may have already installed the same payload.
        // A complete content-addressed winner is a cache hit; the bytes are
        // content-addressed, so whatever is there is what this was going to
        // put there.
        let _ = std::fs::remove_file(&temp);
        if !std::fs::metadata(&path).is_ok_and(|meta| meta.len() == bytes.len() as u64) {
            return Err(e).with_context(|| format!("renaming into {}", path.display()));
        }
        return Ok(key.to_string());
    }

    sweep_occasionally(dir, bytes.len() as u64, &path);
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

/// Write `bytes` to a path nothing is using, refusing any existing entry.
fn write_new(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let create = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
    };
    let mut file = match create() {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(path)?;
            create()?
        }
        other => other?,
    };
    file.write_all(bytes)
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
///
/// It used to accept any existing entry that `Path::is_dir` answered for,
/// which follows a symlink and asks nothing about owner or mode — so a
/// `media` planted by another local account was taken as the cache and every
/// attachment written into it. And the keys here are derived from content the
/// account has already published, which makes them predictable: a file left
/// under one while the directory was open is served as the attachment it
/// names. So a directory found open is swept of what this daemon did not write,
/// which is the whole of what an open directory can have gained.
///
/// Reachable from outside this module because `put` is not the only thing
/// that makes this directory: the daemon prepares it once at startup, and the
/// web bridge's staging write goes through it too. An account that stages
/// uploads and never caches a download used to keep the umask's mode here for
/// ever, with nothing ever sweeping it.
pub(crate) fn prepare_dir(dir: &std::path::Path) -> Result<()> {
    if crate::private_dir::prepare(dir, "cached media")? == crate::private_dir::Found::WasOpen {
        log::warn!(
            "{} was reachable by other accounts on this machine; dropping what this daemon did not put there",
            dir.display()
        );
        // What another account left, and only that. Emptying the directory was
        // the wrong shape of the same sentence: this runs on the first cache
        // write, the front end's own `stage` creates the directory with
        // `create_dir_all` and so with the umask's mode, and the sweep then
        // deleted the `u-` payload of a recording the send had not run yet --
        // which is the one thing under this roof with no other copy. Ownership
        // is the exact test, and a planted file is owned by whoever planted
        // it: the same sweep the plugin directory takes.
        crate::private_dir::drop_foreign_entries(dir)?;
    }
    // Nothing else here, and deliberately: this is on the path of every
    // cached byte range, so it has to stay the two syscalls above — a `mkdir`
    // that usually says `AlreadyExists` and one `symlink_metadata`. The
    // orphan sweep used to be called from here "once, at the first write",
    // which the code never did: `put` is this function's only caller, so it
    // ran a `read_dir` and a `stat` per file for every write, under the wipe
    // lock every entry point holds — exactly the per-write directory walk
    // `SWEEP_INTERVAL_BYTES` exists to avoid. It runs on a schedule instead;
    // see `reclaim_abandoned_writes_periodically`.
    Ok(())
}

/// Delete the writes nobody came back for.
///
/// Three kinds, and they are one rule: a staged upload, the partial of one,
/// and a download this daemon was writing. Every one of them is spared the
/// budget sweep — the bytes are the only copy, or are not all there yet — so
/// every one of them needs an age rule somewhere, or being spared means never
/// being taken at all.
///
/// Split out of [`sweep`] because it has to be reachable without it. The
/// sweep runs on a *cache-write* threshold, and a staged upload is written by
/// the front end rather than through `put`, so it never advances that
/// counter: on an account that downloads no media, an orphan would sit past
/// the allowance for ever with the rule that names it never running.
///
/// So it runs on a schedule of its own rather than as a side effect of
/// somebody else's work — see [`reclaim_abandoned_writes_periodically`],
/// which is what the daemon starts, and [`sweep`], which does it once more
/// when it is about to read the directory anyway. What is *not* an occasion
/// is the write path: it used to be, under a comment claiming this ran once,
/// and `put` is `prepare_dir`'s only caller — so every cached byte range paid
/// a `read_dir` and a `stat` per file, of which a 512 MiB budget allows
/// thousands, with the wipe lock held. Attaching a front end externalizes
/// media once per message that carries it, which made that quadratic in the
/// size of the account.
///
/// Off the caller's loop, because the walk is `std::fs` and its callers are
/// on one: the reactor that answers a staging `PUT` is the same one serving
/// every other front end. Through `unblock` rather than `spawn_blocking` for
/// the reason `session_bridge` gives — what decides where work goes is what
/// the work is, not where the code lives.
///
/// Through [`prepare_dir`] first, and not only to create the directory: this
/// is now the first thing in the process to open it, and `read_dir` follows a
/// symlink. A `media` planted by another local account would otherwise be
/// walked — and unlinked in — before anything asked whose it was.
async fn reclaim_abandoned_writes() {
    let Some(dir) = oxidezap_ipc::media_dir() else {
        return;
    };
    let walked = oxidezap_session::unblock(move || {
        prepare_dir(&dir)?;
        let _wipe = super::WIPE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        sweep(&dir)
    })
    .await;
    match walked {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log::warn!("the media cache could not be swept of abandoned writes: {e:#}"),
        Err(_) => log::warn!("the media cache was not swept of abandoned writes"),
    }
}

/// How often the daemon looks for writes nobody came back for.
///
/// Far below the shorter of the two allowances the walk applies — an hour for
/// a download whose writer is gone — so an orphan is collected promptly after
/// it becomes one rather than at the next restart, and far above what the
/// walk costs, which is one `stat` per cached file every quarter of an hour.
const RECLAIM_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Look for abandoned writes now, and every [`RECLAIM_INTERVAL`] after.
///
/// A task of the daemon's own rather than a call from wherever an orphan is
/// made, because the places one is made are not places that can afford the
/// walk, and one of them the daemon never sees: a desktop front end stages a
/// payload by writing the file itself, so no request passes through here at
/// all. The first pass covers what the run before this one left behind — the
/// `w-` of a download that was in flight when the process died is nobody's,
/// since there is no second daemon coming back for it — and the schedule
/// covers everything after.
///
/// Never returns. The daemon spawns it and the runtime ends it, which is the
/// same lifetime the account has.
pub async fn reclaim_abandoned_writes_periodically() {
    loop {
        reclaim_abandoned_writes().await;
        oxidezap_session::sleep(RECLAIM_INTERVAL).await;
    }
}

static ACTIVE_WRITES: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

pub(crate) struct WriteGuard(PathBuf);

pub(crate) fn track_write(path: PathBuf) -> WriteGuard {
    ACTIVE_WRITES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path.clone());
    WriteGuard(path)
}

impl Drop for WriteGuard {
    fn drop(&mut self) {
        ACTIVE_WRITES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

/// The walk itself, for a caller that already has the directory and a thread
/// to do it on.
fn reclaim_abandoned_writes_in(dir: &std::path::Path) {
    #[cfg(test)]
    walks::record();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // A partial upload can be retried; a completed staged recording may
        // be its only copy, so age alone never makes that file disposable.
        let allowance = if is_staging_partial(&name) {
            STALE_PARTIAL
        } else if is_in_progress(&name) {
            super::IN_PROGRESS_GRACE
        } else {
            continue;
        };
        let active = ACTIVE_WRITES.lock().unwrap_or_else(|e| e.into_inner());
        if active.contains(&entry.path()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        if meta
            .modified()
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age > allowance)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// How many times this thread has walked the media directory looking for
/// orphans.
///
/// Test-only, and instrumentation rather than behaviour: what this module's
/// split is *for* is a cost, and a cost is not visible in what the files
/// afterwards look like. A wall-clock measurement would be the other way to
/// see it and would assert nothing — so the walks are counted, and a burst of
/// cache writes is asserted to do none of them.
///
/// Per thread rather than one static, because the suite runs these in
/// parallel: another test sweeping its own temporary directory is a walk this
/// one must not see. What it therefore counts is the walks a thread does
/// itself, which is the whole of the question being asked — whether a call
/// walks the directory before it returns. The scheduled pass does its walk on
/// a thread of its own and is invisible here, which is what "off the caller's
/// loop" means.
#[cfg(test)]
mod walks {
    use std::cell::Cell;

    thread_local! {
        static COUNT: Cell<u64> = const { Cell::new(0) };
    }

    pub(super) fn record() {
        COUNT.with(|count| count.set(count.get() + 1));
    }

    pub(super) fn so_far() -> u64 {
        COUNT.with(Cell::get)
    }
}

/// How much has been written since the last budget sweep.
///
/// At module scope rather than inside [`sweep_occasionally`] so a test can
/// put it back to a known value: it is process-wide, and a test that asserts
/// what one write does would otherwise be answering for every write the
/// binary has made before it.
static SINCE_SWEEP: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);

/// Drop the oldest files once enough has been written to be worth looking.
static WRITES_SINCE_SWEEP: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);

fn sweep_occasionally(dir: &std::path::Path, written: u64, arriving: &std::path::Path) {
    use std::sync::atomic::Ordering;

    let before = SINCE_SWEEP.fetch_add(written, Ordering::Relaxed);
    let writes = WRITES_SINCE_SWEEP.fetch_add(1, Ordering::Relaxed);
    if before.saturating_add(written) < SWEEP_INTERVAL_BYTES && writes + 1 < SWEEP_INTERVAL_WRITES {
        return;
    }
    SINCE_SWEEP.store(0, Ordering::Relaxed);
    WRITES_SINCE_SWEEP.store(0, Ordering::Relaxed);

    if let Err(e) = sweep_with_limits(dir, Some(arriving), CACHE_BUDGET_BYTES, MAX_CACHE_FILES) {
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
    usage_for("")
}

/// What one account's slice of the shared cache occupies.
///
/// The physical directory is one, so a storage pane that billed this account
/// for the whole walk reported every other account's photos as its own. The
/// account prefix is exactly what `AccountMedia` writes, and stripping it is
/// the same operation [`delete_for`] does, so a wiped account's usage and a
/// measured account's usage agree about what is theirs.
pub fn usage_for(prefix: &str) -> (u64, u64) {
    let Some(dir) = oxidezap_ipc::media_dir() else {
        return (0, 0);
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return (0, 0);
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let local = name.to_string_lossy();
            if !prefix.is_empty() && !local.starts_with(prefix) {
                return None;
            }
            entry.metadata().ok().filter(|meta| meta.is_file())
        })
        .fold((0, 0), |(bytes, files), meta| {
            (bytes + meta.len(), files + 1)
        })
}

/// Delete the files belonging to no account: the pre-multi-account names.
///
/// `scope.takes` is applied to the whole name, which is the local name here
/// because there is no prefix to strip. Best-effort per entry: one unreadable
/// file must not abandon the rest. The lock and the epoch belong to
/// [`super::wipe_for`], which is the only caller.
pub(super) fn delete_unscoped(scope: Wipe) -> Result<()> {
    let Some(dir) = oxidezap_ipc::media_dir() else {
        return Ok(());
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let mut removed = 0usize;
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if oxidezap_ipc::account_prefix_of(&name).is_some() {
            continue;
        }
        // A bare staged key is the daemon's now, not this account's: plugin
        // installation is global and reads `u-<name>` from whichever front end
        // asked, so a legacy-account wipe must not take a payload another
        // account's install is still going to consume. A pre-multi-account
        // *send* payload shares the spelling and cannot be told apart, so it is
        // left until its owning lifecycle consumes or explicitly removes it.
        if oxidezap_ipc::is_global_staged_key(&name) {
            continue;
        }
        if !scope.takes(&name) {
            continue;
        }
        if std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    log::info!(
        "cleared {removed} pre-multi-account media files ({scope:?}) from {}",
        dir.display()
    );
    Ok(())
}

/// Delete only files in one account namespace when `prefix` is non-empty.
pub(super) fn delete_for(prefix: &str, scope: Wipe) -> Result<()> {
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
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(local_name) = name.strip_prefix(prefix) else {
            continue;
        };
        if !scope.takes(local_name) {
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
    sweep_with_limits(dir, None, CACHE_BUDGET_BYTES, MAX_CACHE_FILES)
}

fn sweep_with_limits(
    dir: &std::path::Path,
    arriving: Option<&std::path::Path>,
    budget: u64,
    limit: usize,
) -> Result<()> {
    reclaim_abandoned_writes_in(dir);
    // First retain at most the newest `limit` candidates. Delete only after
    // all ages are known, so read_dir order cannot favor older attachments.
    let mut files = std::collections::BinaryHeap::new();
    let mut held = 0u64;
    let mut arriving_size = None;
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() || !is_disposable(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let path = entry.path();
        if arriving == Some(path.as_path()) {
            arriving_size = Some(meta.len());
            continue;
        }
        held = held.saturating_add(meta.len());
        files.push(std::cmp::Reverse((
            meta.modified().unwrap_or(std::time::UNIX_EPOCH),
            meta.len(),
            path,
        )));
        if files.len() > limit
            && let Some(std::cmp::Reverse((_, len, _))) = files.pop()
        {
            held = held.saturating_sub(len);
        }
    }
    while held.saturating_add(arriving_size.unwrap_or(0)) > budget
        || files.len() + usize::from(arriving_size.is_some()) > limit
    {
        let Some(std::cmp::Reverse((_, len, _))) = files.pop() else {
            break;
        };
        held = held.saturating_sub(len);
    }
    let keep: std::collections::HashSet<_> = files
        .into_iter()
        .map(|std::cmp::Reverse((_, _, path))| path)
        .collect();
    let mut removal_error = None;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if arriving == Some(path.as_path())
            || keep.contains(&path)
            || !entry.file_type().is_ok_and(|kind| kind.is_file())
            || !is_disposable(&entry.file_name().to_string_lossy())
        {
            continue;
        }
        if let Err(error) = std::fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            removal_error.get_or_insert(error);
        }
    }
    if let Some(error) = removal_error {
        return Err(error).context("reclaiming disposable media");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eviction_uses_global_age_order_instead_of_directory_order() {
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let dir = std::env::temp_dir().join(format!(
                "oxidezap-eviction-{}-{order:?}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let specs = [
                ("f-newest", 3, 30),
                ("f-middle", 3, 20),
                ("f-oldest", 1, 10),
            ];
            for index in order {
                let (name, size, age) = specs[index];
                let path = dir.as_path().join(name);
                std::fs::write(&path, vec![1; size]).unwrap();
                let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
                file.set_times(
                    std::fs::FileTimes::new()
                        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(age)),
                )
                .unwrap();
            }
            sweep_with_limits(dir.as_path(), None, 5, 8).unwrap();
            assert!(dir.as_path().join("f-newest").exists());
            assert!(!dir.as_path().join("f-middle").exists());
            assert!(!dir.as_path().join("f-oldest").exists());
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn readers_observe_complete_payloads_or_cache_misses_during_replacement_and_eviction() {
        let dir = std::env::temp_dir().join(format!(
            "oxidezap-read-evict-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f-current");
        let payload_a = vec![1u8; 4096];
        let payload_b = vec![2u8; 8192];
        std::fs::write(&path, &payload_a).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader_stop = std::sync::Arc::clone(&stop);
        let reader_path = path.clone();
        let (started, running) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            assert_eq!(std::fs::read(&reader_path).unwrap(), vec![1u8; 4096]);
            started.send(()).unwrap();
            while !reader_stop.load(std::sync::atomic::Ordering::Relaxed) {
                match std::fs::read(&reader_path) {
                    Ok(bytes) => assert!(bytes == vec![1u8; 4096] || bytes == vec![2u8; 8192]),
                    Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::NotFound),
                }
            }
        });
        running.recv().unwrap();
        for i in 0..16 {
            let _wipe = super::super::WIPE_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let temp = dir.join(format!("w-replace-{i}"));
            let _writing = track_write(temp.clone());
            write_new(&temp, if i % 2 == 0 { &payload_b } else { &payload_a }).unwrap();
            std::fs::rename(&temp, &path).unwrap();
            sweep_with_limits(&dir, None, 0, 0).unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        reader.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bounded_sweep_preserves_saved_staged_and_arriving_bytes() {
        let dir = std::env::temp_dir().join(format!(
            "oxidezap-budget-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "a42-f-old",
            "a42-d-old",
            "a42-a-old",
            "m-legacy",
            "saved-document",
            "a42-u-unsent",
            "u-install",
        ] {
            std::fs::write(dir.join(name), b"synthetic").unwrap();
        }
        let arriving = dir.join("a42-f-arriving");
        std::fs::write(&arriving, b"larger than the fixture budget").unwrap();
        sweep_with_limits(&dir, Some(&arriving), 1, 1).unwrap();
        for name in [
            "saved-document",
            "a42-u-unsent",
            "u-install",
            "a42-f-arriving",
        ] {
            assert!(dir.join(name).exists(), "preserve {name}");
        }
        for name in ["a42-f-old", "a42-d-old", "a42-a-old", "m-legacy"] {
            assert!(!dir.join(name).exists(), "reclaim {name}");
        }
        // Count limits also reclaim zero-byte files after restart.
        for i in 0..6 {
            std::fs::write(dir.join(format!("f-empty-{i}")), b"").unwrap();
        }
        sweep_with_limits(&dir, None, u64::MAX, 2).unwrap();
        let count = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| is_disposable(&entry.file_name().to_string_lossy()))
            .count();
        assert_eq!(count, 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn aged_active_write_is_preserved_then_reclaimed_after_its_owner_retires() {
        let dir = std::env::temp_dir().join(format!(
            "oxidezap-active-write-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("w-synthetic");
        let writing = track_write(path.clone());
        std::fs::write(&path, b"partial cache bytes").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH)
            .unwrap();
        reclaim_abandoned_writes_in(&dir);
        assert!(path.exists());
        drop(writing);
        reclaim_abandoned_writes_in(&dir);
        assert!(!path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A burst of cache writes reads the directory no times at all.
    ///
    /// This is the whole of the split. `prepare_dir` ran the orphan walk
    /// under a comment saying "once, here", but `put` is its only caller, so
    /// "once" was once per cached byte range: a `read_dir` plus a `stat` per
    /// file, on a directory a 512 MiB budget lets grow to thousands of them,
    /// and all of it under the wipe lock every entry point holds — so the
    /// writes serialized behind each other's walks. Attaching a front end
    /// externalizes media once per message that carries it, which made that
    /// quadratic in the size of the account: exactly what
    /// [`SWEEP_INTERVAL_BYTES`] was introduced to prevent.
    ///
    /// Counted rather than timed. A clock would answer differently on a
    /// loaded machine and would assert nothing about why.
    #[test]
    fn a_burst_of_cache_writes_never_walks_the_directory() {
        // Through the entry point, so what is counted is a real cache write
        // with the wipe lock and all: `put` resolves the directory from the
        // environment and there is no seam to hand it a temporary one.
        let Some(dir) = oxidezap_ipc::media_dir() else {
            return; // Nowhere to write, so nothing to count.
        };
        // `put` creates the last component and only that, privately, which
        // is the right shape for a daemon whose enclosing directory is made
        // by whoever logged the user in. A CI runner has no login session and
        // has never run this daemon, so the parent of the media directory is
        // simply absent -- `/run/user/<uid>/oxidezap` on Linux,
        // `$TMPDIR/oxidezap-<uid>` on macOS -- and the single `mkdir` answers
        // `ENOENT`, which is about the machine and not about the walk being
        // counted. Make the parents or say there is nowhere to write; the
        // leaf is still `put`'s to create with 700.
        if dir
            .parent()
            .is_some_and(|parent| std::fs::create_dir_all(parent).is_err())
        {
            return;
        }
        let keys: Vec<String> = (0..16).map(|i| format!("f-3EB0WALKCOUNT{i:02}")).collect();
        let forget = || {
            for key in &keys {
                if let Some(path) = oxidezap_ipc::media_path(key) {
                    let _ = std::fs::remove_file(path);
                }
            }
        };
        // A leftover from an earlier run would answer as a cache hit, and a
        // hit never reaches the write path this is counting.
        forget();
        // And nothing this binary wrote earlier may push these few hundred
        // bytes over the sweep's threshold: a budget sweep walks the
        // directory legitimately, and this is counting the walks that are not
        // legitimate.
        SINCE_SWEEP.store(0, std::sync::atomic::Ordering::Relaxed);
        WRITES_SINCE_SWEEP.store(0, std::sync::atomic::Ordering::Relaxed);

        let before = walks::so_far();
        for key in &keys {
            crate::media::put(key, b"the bytes of a photo").expect("the write lands");
        }
        let walked = walks::so_far() - before;

        forget();
        assert_eq!(
            walked,
            0,
            "{} cache writes walked the media directory {walked} times",
            keys.len()
        );
    }

    /// And the same sentence where every machine can hear it.
    ///
    /// The test above goes through `put`, which is the honest shape but binds
    /// it to a media directory resolved from the environment — and a CI
    /// runner names a path whose parents nobody created, and where making
    /// them is somebody else's to permit the test skips itself. That is the
    /// one place this most needs to be asserted, so it is asserted again a
    /// layer down: the walk was never `put`'s, it was `prepare_dir`'s, and
    /// `prepare_dir` takes the directory as an argument.
    #[test]
    fn preparing_the_directory_never_walks_it() {
        let dir = std::env::temp_dir().join(format!(
            "oxidezap-media-prepare-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        // Something to walk over, so a walk that happened would have work to
        // do rather than answering an empty listing.
        std::fs::create_dir_all(&dir).expect("a temporary directory");
        for i in 0..8 {
            std::fs::write(dir.join(format!("f-3EB0PREPARE{i:02}")), b"a photo").expect("a file");
        }

        let before = walks::so_far();
        for _ in 0..16 {
            super::prepare_dir(&dir).expect("the directory is ours and private");
        }
        let walked = walks::so_far() - before;

        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            walked, 0,
            "preparing the directory walked it {walked} times"
        );
    }

    /// An old completed payload can still be the only copy of an unsent note.
    #[test]
    fn a_staged_upload_is_preserved_even_when_old() {
        // The same shape the tests below use: a named directory rather than
        // a crate added for one case.
        let dir = std::env::temp_dir().join(format!(
            "oxidezap-media-stale-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temporary directory");

        let fresh = dir.join("u-local_1");
        let stale = dir.join("u-local_2");
        let cached = dir.join("f-3EB0ABC");
        for path in [&fresh, &stale, &cached] {
            std::fs::write(path, b"payload").expect("a file");
        }

        // Older than the allowance by a wide margin. Measured back from the
        // file's own timestamp rather than from a clock: `SystemTime::now` is
        // disallowed in this tree, and the file was written a moment ago, so
        // its mtime is the same answer.
        let long_ago = std::fs::metadata(&stale)
            .expect("the staged file")
            .modified()
            .expect("an mtime")
            - super::STALE_PARTIAL
            - std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .expect("the staged file")
            .set_modified(long_ago)
            .expect("an mtime");

        super::sweep(&dir).expect("the sweep runs");

        assert!(fresh.exists(), "a payload a send is still coming for");
        assert!(
            stale.exists(),
            "age cannot prove an unsent payload is disposable"
        );
        assert!(
            cached.exists(),
            "and the cache is under budget, so nothing there is touched"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A partial is the same payload one moment earlier, and is treated as
    /// one: kept while it is being written, taken once nothing is coming back
    /// for it. Sparing it without the age rule leaks an upload whose
    /// connection died mid-`PUT`, since the budget sweep is now told to look
    /// away from it too.
    #[test]
    fn a_partial_upload_is_kept_while_fresh_and_reclaimed_when_abandoned() {
        let dir = std::env::temp_dir().join(format!(
            "oxidezap-media-partial-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temporary directory");

        let prefix = crate::media::STAGING_PARTIAL_PREFIX;
        let writing = dir.join(format!("{prefix}1-u-local_1"));
        let abandoned = dir.join(format!("{prefix}2-u-local_2"));
        for path in [&writing, &abandoned] {
            std::fs::write(path, b"payload").expect("a file");
        }

        let long_ago = std::fs::metadata(&abandoned)
            .expect("the partial")
            .modified()
            .expect("an mtime")
            - super::STALE_PARTIAL
            - std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&abandoned)
            .expect("the partial")
            .set_modified(long_ago)
            .expect("an mtime");

        super::reclaim_abandoned_writes_in(&dir);

        assert!(
            writing.exists(),
            "a rename is still on its way to that payload"
        );
        assert!(!abandoned.exists(), "nothing will ever rename that one");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `w-` name is nobody's to reclaim while its writer is holding it, and
    /// nobody's at all once that writer is gone: no wipe claims one and the
    /// sweep skipped them outright, so a crash between the write and the
    /// rename left a file that nothing on this machine would ever delete.
    #[test]
    fn a_write_whose_process_is_gone_is_reclaimed() {
        let dir = std::env::temp_dir().join(format!("oxidezap-orphan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let orphan = dir.join(format!("{IN_PROGRESS_PREFIX}12345"));
        std::fs::write(&orphan, b"half a photo").unwrap();
        let long_ago = std::time::UNIX_EPOCH
            + std::time::Duration::from_millis(wacore::time::now_millis() as u64)
            - super::super::IN_PROGRESS_GRACE * 2;
        std::fs::File::options()
            .write(true)
            .open(&orphan)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(long_ago))
            .unwrap();

        let live = dir.join(format!("{IN_PROGRESS_PREFIX}67890"));
        std::fs::write(&live, b"a photo being written now").unwrap();

        sweep(&dir).unwrap();

        assert!(!orphan.exists(), "a write nobody is holding stays for ever");
        assert!(live.exists(), "and one still being written is not taken");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two below are unix only: what they set up and assert is a mode,
    /// and Windows has none to read.
    #[cfg(unix)]
    mod modes {
        use std::os::unix::fs::PermissionsExt as _;

        /// The front end's own `stage` makes this directory with `create_dir_all`,
        /// which is the umask's mode and not `0700`, so the daemon's first cache
        /// write finds it open. Emptying it there deleted the staged payload of a
        /// recording whose send had not run yet: the send then failed with the
        /// only copy of the note gone.
        #[test]
        fn repairing_the_cache_keeps_a_recording_waiting_to_be_sent() {
            let dir = std::env::temp_dir().join(format!(
                "oxidezap-media-repair-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

            let staged = dir.join("u-local_audio_1");
            std::fs::write(&staged, b"a voice note").unwrap();

            crate::media::platform::prepare_dir(&dir).unwrap();

            assert!(staged.exists(), "the only copy of the note is still there");
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700,
                "and the directory was still tightened"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A key here is derived from content the account has already published,
        /// so it is predictable. A symlink of the right length planted under one
        /// while the directory was open answered "already cached" before the
        /// sweep that exists to remove it had run, and the daemon then served the
        /// link's target as the account's own media.
        #[test]
        fn a_planted_link_is_not_a_cache_hit() {
            let base = std::env::temp_dir().join(format!(
                "oxidezap-media-plant-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&base);
            let dir = base.join("media");
            std::fs::create_dir_all(&dir).unwrap();

            let elsewhere = base.join("elsewhere");
            std::fs::write(&elsewhere, b"not ours").unwrap();
            let planted = dir.join("f-key");
            std::os::unix::fs::symlink(&elsewhere, &planted).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();

            crate::media::platform::prepare_dir(&dir).unwrap();

            assert!(
                std::fs::symlink_metadata(&planted).is_err(),
                "the link is gone before anything asks whether it is a hit"
            );
            assert!(elsewhere.exists(), "and its target was never followed");
            let _ = std::fs::remove_dir_all(&base);
        }
    }
}
