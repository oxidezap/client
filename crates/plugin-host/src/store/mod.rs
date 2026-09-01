//! Where a plugin host keeps what has to survive a restart.
//!
//! Two things, and only two: what the user allowed each plugin to do, and
//! whatever a plugin kept for itself. Both are small JSON documents named by
//! the host, which is the whole of why one interface can serve a filesystem
//! and a browser's origin storage — nothing here needs a path, a directory
//! listing or a seek.
//!
//! The rules that made the file version worth its size are properties of the
//! *caller* and stay where they were: approvals never live in a plugin's own
//! store, a plugin id can never name the approvals document because the host
//! prefixes its own with `kv-`, and a failed write of a grant is not a grant.
//! What each implementation owes is narrower — hand back what was written, or
//! nothing.

/// A named document that outlives the process.
///
/// `Send + Sync` because the approvals are read from every thread the daemon
/// answers a request on and written from whichever one the user's answer
/// arrived on. That costs a page nothing: the browser implementation holds a
/// prefix and reaches for its global per call, rather than keeping a JS
/// object it could not share anyway.
pub trait Backing: Send + Sync + 'static {
    /// Read `name`, or `None` when it is not there, unreadable, or larger
    /// than `max`.
    ///
    /// The bound is asked of the stored size where the platform can answer
    /// that without reading — a planted file is an allocation, and reading
    /// one to discover how big it is would be the allocation the bound
    /// exists to refuse.
    fn read(&self, name: &str, max: usize) -> Option<Vec<u8>>;

    /// Replace `name`, atomically enough that a host killed mid-write leaves
    /// either the old document or the new one.
    ///
    /// # Errors
    ///
    /// Whatever the platform said, in words, for the one log line each caller
    /// writes about it.
    fn write(&self, name: &str, bytes: &[u8]) -> Result<(), String>;

    /// Remove `name`, durably. Missing is success.
    ///
    /// # Errors
    ///
    /// Whatever the platform said. Fallible because of the one caller that
    /// acts on it: a withdrawal whose write failed removes the document
    /// instead, and a removal that silently failed would leave the old grant
    /// to be read back on the next start while Settings had already drawn the
    /// plugin as revoked.
    fn remove(&self, name: &str) -> Result<(), String>;

    /// What to call `name` in a log line: a path, or an origin's storage.
    fn describe(&self, name: &str) -> String;

    /// Whether an answer written here will still be here next time.
    ///
    /// Asked of the store a reload installs, because the approvals are the
    /// *host's* and survive the swap: a set loaded against a directory that
    /// has since been refused would otherwise keep grants that nothing can
    /// record, which is the one direction `usable_state_dir` exists to
    /// prevent — it refuses rather than trusts, and refusing has to mean
    /// every plugin is unapproved until somebody says so again.
    ///
    /// True by default: a store that cannot say otherwise is one that keeps
    /// what it is given.
    fn keeps_answers(&self) -> bool {
        true
    }
}

/// A store with nowhere to write.
///
/// What a host with no usable state directory gets, and what a test wants.
/// Reads answer nothing and writes succeed, so a plugin keeps its settings
/// for the life of the process and the approvals are asked for again — which
/// is the safe direction for the one of the two that is authority.
pub struct Nowhere;

impl Backing for Nowhere {
    fn read(&self, _name: &str, _max: usize) -> Option<Vec<u8>> {
        None
    }

    fn write(&self, _name: &str, _bytes: &[u8]) -> Result<(), String> {
        Ok(())
    }

    fn remove(&self, _name: &str) -> Result<(), String> {
        Ok(())
    }

    fn describe(&self, name: &str) -> String {
        format!("{name} (kept in memory only)")
    }

    /// Nothing written here outlives the process, which is exactly what a
    /// refused state directory means and what makes it fail closed.
    fn keeps_answers(&self) -> bool {
        false
    }
}

#[cfg_attr(target_family = "wasm", path = "web.rs")]
#[cfg_attr(not(target_family = "wasm"), path = "native.rs")]
mod platform;

// Glob because the two halves export different names rather than one
// interface twice — `Files` on a desktop, `Origin` on a page — so there is no
// pair to name here, and naming them would put back the `cfg` this module is
// arranged to do without.
pub use platform::*;
