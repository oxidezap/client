//! See [`super`]: the message, and nothing behind it.

use oxidezap_ipc::DaemonMessage;

use crate::state::StateHub;

/// Ask whoever is attached to raise their window.
///
/// There is no second half here. Launching a front end is starting a process,
/// and the only front end a page has is the page — which, if it is not
/// listening for this, is not running at all.
pub fn show(hub: &StateHub) {
    hub.signal(&DaemonMessage::ShowWindow);
}
