//! What the window needs from underneath it, once there may be no operating
//! system there.
//!
//! Everything in here has the same shape: one function the interface calls,
//! two implementations behind it, and no `cfg` anywhere above. A component
//! never learns that browsers exist for the same reason it never learns that
//! small screens do.
//!
//! The shape is spelled the same way in every file: a public function with
//! the documentation on it, and one `#[cfg]`-selected `mod imp` pair holding
//! the two answers. The name is `imp` and not the platform because the
//! *dispatch* has to be one name — `imp::save(..)` compiles on both, where
//! `native::save(..)` would need a `cfg` of its own at the call, which is the
//! very thing this module exists to keep out. Which platform an `imp` is is
//! on the line directly above it.
//!
//! One pair per file, holding every answer that file makes. A second pair
//! cannot be called `imp` too, and a file that grew one ended up with three
//! module names for one idea. A half becomes its own *file* only when it
//! grows submodules or stops fitting beside the question it answers — as
//! `oxidezap-audio`'s and `oxidezap-video`'s web backends have. Nothing here
//! is near that: these are two-line answers, and a file per platform would
//! put the question and its answers in three places.
//!
//! The one file without a pair is [`clock`], where each half is a single
//! expression and the `#[cfg]` is on the two blocks themselves. A module to
//! hold one line is ceremony; the rule starts where there is a body to name.

mod capabilities;
pub mod clock;
pub mod download;
pub mod fonts;
pub mod identity;
pub mod launch;
pub mod lifecycle;
pub mod log_store;
pub mod picker;
pub mod plugins;
pub mod prefs;
pub mod startup;

pub use capabilities::{calls_belong_to_another_tab, calls_unavailable, video_decode_unavailable};
pub use clock::{sleep, with_timeout};
pub use fonts::fonts;
pub use identity::front_end_id;
pub use launch::run;
pub use lifecycle::watch_for_departure;
pub use plugins::Home as PluginHome;
pub use startup::{application, clocks, logging};
