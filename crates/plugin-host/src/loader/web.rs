//! A page has no folder to look in.
//!
//! Nothing here scans, reads or checks a mode, because a browser hands the
//! host its modules rather than letting it go and find them: `plugins::web`
//! in the daemon takes them out of the origin's storage and calls
//! [`Plugins::start`](crate::Plugins::start) with what it holds. The
//! permission checks the desktop half is mostly made of have no counterpart
//! either — what stands in for "only this user can write it" is the origin,
//! which no other site can reach at all.
//!
//! So this file is one function, and it is the one the page needs for the
//! opposite reason to the desktop's. A stub for each of the others would be
//! a filesystem interface a page could call and get a lie from; the parent
//! module offers them only where they exist.

use portable_atomic::AtomicU32;

/// A page has neither half of the problem this answers.
///
/// Nothing here can block — everything is on one agent, so waiting for a
/// task that can only run when this returns is a hang rather than a wait
/// — and there is nothing to wait *for*: the wipe a desktop is racing is
/// a directory being deleted, and an origin's storage is ordered by the
/// stamp instead, which refuses a retired handle's writes outright.
pub fn wait_for_any_reload(_reload: &AtomicU32) {}
