//! Whether the daemon this front end talks to can run plugins at all.
//!
//! Not "are there any", which the plugin list already answers. A page that
//! runs its own session has a daemon with no threads and no filesystem, so
//! its plugin list is empty for a reason no amount of installing will change
//! — and "None loaded: drop a .wasm in the plugins folder and restart" is
//! then advice about a folder that does not exist, given to somebody who
//! cannot act on it.
//!
//! The daemon half of this is `daemon::plugins::start`, and the two have to
//! agree: this decides what is drawn where a list would be, and that decides
//! what is loaded.

/// Why this front end's daemon runs no plugins, or `None` if it can.
///
/// A sentence, because it is drawn as one.
#[must_use]
pub fn plugins_unavailable() -> Option<&'static str> {
    imp::plugins_unavailable()
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    /// A desktop front end reaches `oxidezapd`, which has both halves.
    pub fn plugins_unavailable() -> Option<&'static str> {
        None
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    /// A page attached to a real daemon has that daemon's plugins: the web
    /// bridge hands `serve_client` the same host the socket does, so the
    /// interface, the approvals and the actions all travel the protocol they
    /// already travel. It is only a page holding the session *itself* that
    /// has none — asked the same way the session asks it, so the two cannot
    /// answer differently.
    pub fn plugins_unavailable() -> Option<&'static str> {
        match oxidezap_ipc::web::named_daemon() {
            oxidezap_ipc::web::NamedDaemon::Named(_) => None,
            // Rejected is not "no daemon": the window is on the settled
            // refusal screen and is drawing no Settings at all. Answered with
            // the same sentence as `Nobody` rather than a third case, because
            // a case nothing can reach is a case nobody maintains.
            _ => Some(
                "Plugins run in the daemon, and this page is its own: a plugin \
                 gets a thread and a folder, and a tab has neither. Point this \
                 page at an oxidezapd with #daemon=ws://… and its plugins \
                 appear here.",
            ),
        }
    }
}
