//! What the window needs from underneath it, once there may be no operating
//! system there.
//!
//! Everything in here has the same shape: one function the interface calls,
//! two implementations behind it, and no `cfg` anywhere above. A component
//! never learns that browsers exist for the same reason it never learns that
//! small screens do.

mod capabilities;
pub mod clock;
pub mod download;
pub mod fonts;
pub mod identity;
pub mod launch;
pub mod lifecycle;
pub mod log_store;
pub mod plugins;
pub mod prefs;
pub mod startup;

pub use capabilities::{
    calls_belong_to_another_tab, calls_unavailable, media_send_unavailable,
    video_decode_unavailable,
};
pub use clock::{sleep, with_timeout};
pub use fonts::fonts;
pub use identity::front_end_id;
pub use launch::run;
pub use lifecycle::watch_for_departure;
pub use plugins::Home as PluginHome;
pub use startup::{application, clocks, logging};
