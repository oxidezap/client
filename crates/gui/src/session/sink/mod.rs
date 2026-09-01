//! Where the reader puts what it has read.
//!
//! One queue, two disciplines, because the two readers are not the same kind
//! of thing. A native front end reads on a thread of its own, so a full queue
//! is a reason to *stop reading* — the daemon then overruns its own bounded
//! broadcast and says `Resync`, which is the recovery this protocol already
//! has. A page has one thread, and it is the thread that drains this queue:
//! blocking on it would park the only thing that could empty it, so the queue
//! is unbounded there and the back pressure has nowhere to come from anyway.
//!
//! Both ends are the same two methods, so nothing above this knows which —
//! and since the whole of each end *is* those two methods and the channel
//! behind them, the split is a file each rather than a `#[cfg]` on every
//! item. What the two share is the name, which is why this file holds no code
//! at all.

#[cfg(not(target_family = "wasm"))]
mod native;
#[cfg(target_family = "wasm")]
mod web;

#[cfg(not(target_family = "wasm"))]
pub use native::{EventSink, Events, channel};
#[cfg(target_family = "wasm")]
pub use web::{EventSink, Events, channel};
