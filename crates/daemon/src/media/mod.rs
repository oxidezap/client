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

/// Delete the cached files this wipe is entitled to.
///
/// Part of "clear data and pair again", and not optional there: the store is
/// one file, but the media beside it is a directory that can hold half a
/// gigabyte of the *previous* account's photos, videos and documents. Leaving
/// it in place means pairing a different account onto a cache of someone
/// else's pictures, with no control anywhere that clears them.
///
pub use platform::wipe;

pub use platform::{cache_usage, claim, has, put, put_owned, take};
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

/// A payload a front end staged for a send that has not run yet.
///
/// The one thing under this roof that is not a cache: there is no other copy,
/// so nothing may drop it to reclaim space. Asked directly rather than through
/// [`Wipe::Cache`], which is deliberately narrower — it names the two prefixes
/// a "clear cached media" is entitled to, and the budget sweep has to reclaim
/// more than that.
pub(super) fn is_staged_upload(name: &str) -> bool {
    name.starts_with("u-")
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
pub fn put_since(epoch: usize, key: &str, bytes: &[u8]) -> Result<String> {
    // Held across the check *and* the write, so a wipe cannot land between
    // them. See `WIPE_LOCK`.
    let _guard = WIPE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if CACHE_EPOCH.load(Ordering::SeqCst) != epoch {
        anyhow::bail!("the media cache was cleared while this was being prepared");
    }
    platform::put_evictable(key, bytes)
}
