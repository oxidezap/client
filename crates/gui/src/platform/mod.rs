//! What the window needs from underneath it, once there may be no operating
//! system there.
//!
//! Everything in here has the same shape: one function the interface calls,
//! two implementations behind it, and no `cfg` anywhere above. A component
//! never learns that browsers exist for the same reason it never learns that
//! small screens do.

pub mod clock;
pub mod prefs;

pub use clock::{sleep, with_timeout};
