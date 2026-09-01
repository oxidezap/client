//! Where a frame's bytes live between the daemon and its front end.
//!
//! A frame names media by a key rather than carrying it: a history load is a
//! hundred chats, and a photo in each of them would be a frame nobody could
//! send. What the key names is this module's business, and it is the one
//! thing about media that differs by platform — a directory both processes
//! can open, or a map the one process holds.
//!
//! Everything else is written once: what a key *is*, what a wipe may take,
//! and the epoch that keeps a writer from filling a cache somebody just
//! cleared.

#[cfg_attr(target_family = "wasm", path = "web.rs")]
#[cfg_attr(not(target_family = "wasm"), path = "native.rs")]
mod platform;

/// The same three verbs over HTTP, for the front end that shares no
/// filesystem with the daemon. Native only: a page's own daemon hands its
/// bytes over in memory, and there is no port to serve them on.
#[cfg(not(target_family = "wasm"))]
pub(crate) mod http;

/// The orphan sweep, called by the daemon at startup as well as from the
/// repair below. Native only: a page has no directory to walk.
#[cfg(not(target_family = "wasm"))]
pub use platform::reclaim_abandoned_writes;
pub use platform::{cache_usage, claim, has, take};
/// Read without removing, where the front end is this process. See `web.rs`.
#[cfg(target_family = "wasm")]
pub use platform::{deliver, read};

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;

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
/// `None` when there is no identity to address by: the field comes off the
/// network and is a plain `Vec<u8>`, so it can arrive short or empty, and
/// truncating whatever is there produced the literal key `d-` for every one
/// of them. Two unrelated messages then shared a cache entry, and a download
/// of the second was answered with the first one's bytes.
pub fn download_key(file_enc_sha256: &[u8]) -> Option<String> {
    if file_enc_sha256.len() < KEY_HASH_BYTES {
        return None;
    }
    let mut key = String::from("d-");
    for byte in file_enc_sha256.iter().take(KEY_HASH_BYTES) {
        use std::fmt::Write as _;
        let _ = write!(key, "{byte:02x}");
    }
    Some(key)
}

/// How much of the hash the key carries, and the least it may be built from.
/// 128 bits is far past collision range for a cache and keeps the name short
/// enough to read.
const KEY_HASH_BYTES: usize = 16;

/// Spell a key in the characters [`oxidezap_ipc::media_path`] accepts,
/// without letting two ids meet under one name.
///
/// Message ids come off the network. One carrying a separator would name a
/// file outside the cache, and the daemon writes as the user who owns the
/// session. The earlier answer folded every character outside
/// `[A-Za-z0-9_-]` onto `.`, which kept the file inside the cache and lost
/// the difference between ids: `3EB0A/B` and `3EB0A?B` were one key. A
/// message key is a content address, so the second message would have been
/// served the first one's bytes — which is the failure [`download_key`]
/// above already carries the scar of.
///
/// So this escapes rather than folds: a byte outside that set becomes `.`
/// and its two hex digits, and `.` escapes itself, which makes the spelling
/// reversible and therefore injective. Per byte rather than per character,
/// so a non-ASCII id is distinguished by what it actually is.
///
/// An id of the alphabet WhatsApp actually uses has nothing to escape, so it
/// passes through byte for byte and every entry already on disk keeps the
/// name it was written under. The two spellings do share a directory for as
/// long as those entries live: a fold wrote `.` where this writes `.2E`, so
/// an entry an older build cached under an id carrying punctuation could be
/// read as some other id's now. It takes an id no WhatsApp build produces to
/// have written one, and the budget sweep retires whatever did; changing the
/// prefix instead — the answer this key's own `m-` history records — would
/// orphan every cached photo on every disk to answer an id nobody has seen.
///
/// Nothing is truncated, and a key that does not fit a file name is left not
/// fitting. `media_path` refuses a name over 128 bytes, so an id that long —
/// escaped to three bytes a character, or simply written long — is a message
/// whose media is fetched on demand every time instead of cached. That is the
/// price of the rule: a key cut to a length is some *other* id's address, and
/// this function exists because two ids sharing one key is how a photo is
/// served as the wrong message's.
fn sanitize(id: &str) -> String {
    use std::fmt::Write as _;

    let mut key = String::with_capacity(id.len());
    for byte in id.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' {
            key.push(char::from(byte));
        } else {
            let _ = write!(key, ".{byte:02X}");
        }
    }
    key
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
    pub(super) fn takes(self, name: &str) -> bool {
        match self {
            Self::Everything => true,
            Self::Cache => {
                (name.starts_with("f-") || name.starts_with("d-")) && !is_in_progress(name)
            }
        }
    }
}

/// How long a write may sit unfinished before it is an orphan.
///
/// A live one holds its name for one write and one rename, so anything this
/// old is a file whose writer is gone: a crash between the two leaves a `w-`
/// that no wipe claims and the sweep skips, which is a leak the `.partN`
/// name it replaced did not have.
#[cfg_attr(
    target_family = "wasm",
    expect(
        dead_code,
        reason = "a page's cache is a map, and has no temporaries to reclaim"
    )
)]
pub(super) const IN_PROGRESS_GRACE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// A payload a front end staged for a send that has not run yet.
///
/// The one thing under this roof that is not a cache: there is no other copy,
/// so nothing may drop it to reclaim space. Asked directly rather than through
/// [`Wipe::Cache`], which is deliberately narrower — it names the two prefixes
/// a "clear cached media" is entitled to, and the budget sweep has to reclaim
/// more than that.
pub(super) fn is_staged_upload(name: &str) -> bool {
    oxidezap_ipc::is_staged_key(name)
}

/// What a staged upload is called while it is still being written.
///
/// The write goes to this name and is renamed onto the key, so that the key
/// holds the whole payload or nothing. Named with a leading dot because
/// `media_path` refuses one, which is what keeps a caller from reading a
/// half-written payload or deleting it out from under the rename.
#[cfg(not(target_family = "wasm"))]
pub(crate) const STAGING_PARTIAL_PREFIX: &str = ".staging-";

/// A staged upload that has not finished crossing the bridge.
///
/// It has to be spared and reclaimed for the same two reasons a finished one
/// is, and neither is served by leaving it to the budget sweep's default. It
/// is the only copy of a payload somebody is waiting to send, so evicting it
/// mid-write breaks the rename and fails a voice note over a full cache; and
/// it counts toward no budget it could be dropped for, since a partial is
/// written by the front end rather than through `put`. But sparing alone
/// leaks: a connection that dies mid-`PUT` leaves one with nothing else to
/// remove it, which is why the age rule takes these too.
#[cfg(not(target_family = "wasm"))]
pub(super) fn is_staging_partial(name: &str) -> bool {
    name.starts_with(STAGING_PARTIAL_PREFIX)
}

/// The prefix a write in progress carries.
///
/// Its own, and not the key's: named `<key>.partN`, a temporary inherited the
/// prefix of what it was becoming, so a "clear cached media" and the budget
/// sweep both counted a download somebody was waiting for as theirs to
/// delete. Nothing claims this one — a wipe of everything does, which is the
/// account leaving, and the epoch already answers for that.
pub(super) const IN_PROGRESS_PREFIX: &str = "w-";

/// Whether a file is a write that has not landed yet.
pub(super) fn is_in_progress(name: &str) -> bool {
    name.starts_with(IN_PROGRESS_PREFIX)
}
/// Which cache the writers still in flight think they are writing into.
///
/// A download dispatched before a wipe finishes after it, and the eager cache
/// of an inbound message can be queued across one. Neither can be cancelled,
/// so the answer is the same as everywhere else in this codebase: bump a
/// number and let the writer notice.
pub(super) static CACHE_EPOCH: AtomicUsize = AtomicUsize::new(0);

/// Held across a wipe, and across an epoch-checked write.
///
/// The epoch alone is only a check-then-act: an eager writer could read a
/// matching epoch, a wipe could then bump it and delete everything, and the
/// writer's rename could land afterwards — repopulating a directory the user
/// had just been told was empty. Nothing else in this module needs the lock,
/// because nothing else claims to be ordered against a wipe.
pub(super) static WIPE_LOCK: Mutex<()> = Mutex::new(());

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
pub fn put(key: &str, bytes: &[u8]) -> Result<String> {
    // Here rather than in the backend, for the reason `wipe` states: the lock
    // belongs to the entry points. A backend that took it too would deadlock
    // the one caller that already holds it — `put_since`, below — and that
    // caller is every inbound message with media in it.
    let _guard = WIPE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    platform::put(key, bytes)
}

/// The same, for a caller that already owns the only copy. See
/// [`platform::put_owned`].
///
/// # Errors
///
/// Whatever the platform's write answers.
pub fn put_owned(key: &str, bytes: Vec<u8>) -> Result<String> {
    let _guard = WIPE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    platform::put_owned(key, bytes)
}

pub fn put_since(epoch: usize, key: &str, bytes: &[u8]) -> Result<String> {
    // Held across the check *and* the write, so a wipe cannot land between
    // them. See `WIPE_LOCK`.
    let _guard = WIPE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if CACHE_EPOCH.load(Ordering::SeqCst) != epoch {
        anyhow::bail!("the media cache was cleared while this was being prepared");
    }
    platform::put_evictable(key, bytes)
}

/// Delete the cached files this wipe is entitled to.
///
/// Part of "clear data and pair again", and not optional there: the store is
/// one file, but the media beside it is a directory that can hold half a
/// gigabyte of the *previous* account's photos, videos and documents. Leaving
/// it in place means pairing a different account onto a cache of someone
/// else's pictures, with no control anywhere that clears them.
///
/// The lock and the epoch are taken here rather than by each platform, so a
/// backend cannot forget them. One did: the page's wipe emptied its map
/// without moving the epoch, so a publisher still draining its queue found
/// the epoch it was handed still current and put the bytes straight back.
///
/// # Errors
///
/// Whatever the platform's deletion answers.
pub fn wipe(scope: Wipe) -> Result<()> {
    // For the whole wipe, so an epoch-checked write is either wholly before
    // it — and deleted by it — or wholly after, and kept.
    let _guard = WIPE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Before the deletions, not after: a writer that reads the epoch between
    // the two would otherwise believe its file survived the wipe that is
    // about to remove it.
    invalidate();
    platform::delete(scope)
}

/// Retire the epoch every writer in flight is holding.
///
/// Split out so the property can be asserted without deleting anybody's
/// cache. [`wipe`] is its only caller.
fn invalidate() {
    CACHE_EPOCH.fetch_add(1, Ordering::SeqCst);
}

/// What both backends have to answer the same way.
///
/// These live here rather than beside either implementation because that is
/// the whole subject: the page's cache and the daemon's directory are two
/// answers to one question, and the one place they silently disagreed was a
/// wipe that emptied the map without retiring the epoch. Written once, so a
/// third backend inherits them.
#[cfg(test)]
mod tests {
    use super::*;

    /// A publisher's queue outlives the tap that cleared the cache, so the
    /// write it is still holding has to be refused rather than repopulating a
    /// directory the user was just told was empty.
    #[test]
    fn a_write_prepared_before_a_clear_is_refused() {
        let before = epoch();
        invalidate();
        assert!(
            put_since(before, "f-3EB0ABC", b"the bytes of a photo").is_err(),
            "the cache was cleared after this write was prepared"
        );
    }

    /// The lock belongs to the entry point, and only to it.
    ///
    /// A backend that took `WIPE_LOCK` for its own rename deadlocked the one
    /// caller already holding it, which is every inbound message carrying
    /// media: the publish thread stopped there and never came back. On a
    /// thread of its own with a deadline, so the failure is this assertion
    /// rather than a suite that hangs.
    #[test]
    fn a_write_that_checked_the_epoch_still_reaches_the_disk() {
        let Some(dir) = oxidezap_ipc::state_dir() else {
            return; // Nowhere to write, so nothing to race over.
        };
        let _ = std::fs::create_dir_all(&dir);

        let (done, answered) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(put_since(epoch(), "f-3EB0LOCKCHECK", b"a photo"));
        });
        let landed = answered
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the write never answered: the lock was taken twice");

        assert!(landed.is_ok(), "the write itself failed: {landed:?}");
        if let Some(path) = oxidezap_ipc::media_path("f-3EB0LOCKCHECK") {
            let _ = std::fs::remove_file(path);
        }
    }

    /// The epoch is what a writer in flight compares against, so a clear that
    /// leaves it standing is a clear that undoes itself.
    #[test]
    fn clearing_the_cache_retires_the_epoch_every_writer_is_holding() {
        let before = epoch();
        invalidate();
        assert_ne!(epoch(), before);
    }

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

    /// The field comes off the network as a plain byte string, so it can
    /// arrive empty. Truncating whatever is there produced the literal key
    /// `d-` for every one of them, and the second such download was answered
    /// with the first one's bytes.
    #[test]
    fn media_with_no_content_hash_has_no_key() {
        assert_eq!(download_key(&[]), None);
        assert_eq!(download_key(&[0xab; 4]), None);
        assert!(download_key(&[0xab; 16]).is_some());
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

    /// A partial is spared the budget and taken by the age rule, which is the
    /// pairing that matters: sparing alone leaks an upload whose connection
    /// died, and taking alone drops one that is still being written.
    #[test]
    #[cfg(not(target_family = "wasm"))]
    fn a_partial_is_spared_the_budget_and_reclaimed_on_age() {
        let partial = format!("{STAGING_PARTIAL_PREFIX}7-u-local_audio-7");
        assert!(is_staging_partial(&partial));
        assert!(
            !is_staged_upload(&partial),
            "it is not addressable as a key, and the two rules ask separately"
        );
        assert!(!is_staging_partial("u-local_audio-7"));
        assert!(!is_staging_partial("f-3EB0ABC"));
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

    /// A message key is a content address, so two ids sharing one is the
    /// same failure `download_key` records: a hit is served as that message's
    /// own media. Folding every character outside `[A-Za-z0-9_-]` onto `.`
    /// made these pairs one key each.
    #[test]
    fn two_message_ids_never_share_a_key() {
        for (one, other) in [
            ("3EB0A/B", "3EB0A?B"),
            ("3EB0A.B", "3EB0A/B"),
            ("a b", "a-b"),
            ("..", "//"),
            // The same characters, differing only in what they are: an
            // escape is per byte, so this is two ids rather than one.
            ("é", "e\u{301}"),
        ] {
            assert_ne!(
                message_key(one),
                message_key(other),
                "{one} and {other} were cached as one message"
            );
        }
    }

    /// An id too long to spell as a file name loses its cache rather than its
    /// tail: the truncation it replaced handed two ids sharing a prefix one
    /// key, and that is the failure this whole spelling exists to answer. The
    /// message still arrives; its media is fetched on demand.
    #[test]
    fn an_id_too_long_to_name_a_file_is_refused_rather_than_cut() {
        let long = message_key(&"/".repeat(60));
        assert!(
            oxidezap_ipc::media_path(&long).is_none(),
            "an id this long was cut to fit, and the cut is another id's key"
        );
        assert_ne!(long, message_key(&"/".repeat(61)));
    }

    /// The key is a file name on a disk that outlives the process, so a
    /// spelling change orphans every entry written under the old one. An id
    /// of the alphabet WhatsApp uses has nothing to escape and keeps the name
    /// it already has.
    #[test]
    fn an_ordinary_message_id_keeps_the_key_it_was_cached_under() {
        assert_eq!(message_key("3EB0A1B2C3D4E5F6"), "f-3EB0A1B2C3D4E5F6");
        assert_eq!(message_key("A-B_c9"), "f-A-B_c9");
    }

    /// The same media shared into two chats is one file, so the second
    /// download never happens.
    #[test]
    fn the_same_content_is_the_same_key() {
        let sha = [0xab_u8; 32];
        assert_eq!(download_key(&sha), download_key(&sha));
        assert_ne!(download_key(&sha), download_key(&[0xcd; 32]));
        assert!(
            download_key(&sha).expect("a hash").len() < 40,
            "a key is a file name a person may have to read"
        );
    }

    /// Message keys and download keys share a directory and must not collide:
    /// one is addressed by message, the other by content. The message prefix
    /// is `f-` because it also promises *full* media — see [`message_key`].
    #[test]
    fn the_two_key_spaces_stay_apart() {
        assert!(message_key("abc").starts_with("f-"));
        assert!(download_key(&[1; 32]).expect("a hash").starts_with("d-"));
        assert!(
            !message_key("abc").starts_with("m-"),
            "the old prefix cached thumbnails under it and must stay orphaned"
        );
    }
}
