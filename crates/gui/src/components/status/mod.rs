//! Status: the sidebar list of who posted, and the pane that plays them back.

mod list;
mod ring;
mod view;

pub use list::{StatusListProps, render_status_list};
pub use view::{StatusViewProps, render_status_view};

pub(crate) use ring::status_ring;
