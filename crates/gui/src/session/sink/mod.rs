//! Where the reader puts what it has read.
//!
//! One queue, two ends, because the two publishers are not the same kind of
//! thing. The reader is one of them: a native front end reads on a thread of
//! its own, so a full queue is a reason to *stop reading* — the daemon then
//! overruns its own bounded broadcast and says `Resync`, which is the
//! recovery this protocol already has. Everything else that publishes runs
//! *on* the UI executor, and that executor is the thing that drains this
//! queue: waiting for room there parks the only thread that could make any,
//! and the window stops with no error and nothing in the log.
//!
//! Which of the two a caller was got enforced by comments, and one wrong call
//! in a later edit was a hung window. It is enforced by the types now. The
//! reader is handed a [`ReaderSink`], which is the only thing that can wait
//! and is not `Clone` — there is one reader. Everything above it is handed a
//! [`UiSink`], which cannot wait because it has no method that could, and is
//! cloned into every caller that needs one. A [`UiSink`] comes from
//! [`ReaderSink::ui`] and from nowhere else, so the executor's half is always
//! a view onto the reader's queue rather than a second channel.
//!
//! A page has one thread and it is that same thread again, so the queue is
//! unbounded there and neither end can wait anyway. The split still holds on
//! both, because nothing above this may learn which one it is on — so the two
//! files are the same two types with the same two methods, and what they
//! disagree about is only what happens when the queue is full.
//!
//! Since the whole of each end *is* those methods and the channel behind
//! them, the split is a file each rather than a `#[cfg]` on every item. What
//! the two share is the names, and the one thing here that is not a name: the
//! answer a publish that never happened gives back.

#[cfg(not(target_family = "wasm"))]
mod native;
#[cfg(target_family = "wasm")]
mod web;

#[cfg(not(target_family = "wasm"))]
pub use native::{Events, ReaderSink, UiSink, channel};
#[cfg(target_family = "wasm")]
pub use web::{Events, ReaderSink, UiSink, channel};

/// An event that was not published, and why.
///
/// Returned rather than swallowed, because the two reasons are not the same
/// news. `Gone` is a front end that has ended, and is the ordinary way a
/// connection stops mattering. `Full` is a window that fell far enough behind
/// that something it was going to be told — a message that failed to send, a
/// page of history that never came — went on the floor, and whatever is
/// waiting on it waits for good. Nothing above can do better than dropping
/// it: the alternative is waiting on the thread that drains the queue. So
/// this exists to be *said*, which [`UiSink::try_send`] does before handing
/// it back, and to let a test tell the two apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dropped {
    /// Nobody is draining this queue any more.
    Gone,
    /// The front end is behind and the queue has no room. Desktop only: a
    /// page's queue is unbounded, so it can only fail the other way.
    #[cfg_attr(
        target_family = "wasm",
        expect(dead_code, reason = "a page's queue has no ceiling to hit")
    )]
    Full,
}
