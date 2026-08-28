//! A task on the page's own loop, because there is no thread to give it and
//! nothing here that would need one.
//!
//! The media write this drains is an insert into a map in this address space
//! — see [`crate::media`] — so there is nothing blocking to keep away from
//! anything. What is kept is the *shape*: the queue still closes, the drain
//! still ends, and the caller still waits for it before deleting an account's
//! media.

use std::sync::Arc;

use oxidezap_core::UiEvent;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::state::StateHub;

/// The drain, awaited rather than joined.
pub(crate) struct Handle(oxidezap_session::Task<()>);

impl Handle {
    /// Wait for the queue to drain.
    pub(crate) async fn join(self) {
        if self.0.await.is_err() {
            log::error!("the publish task did not finish");
        }
    }
}

/// Start draining `queue` into `hub`.
pub(crate) fn start(hub: Arc<StateHub>, mut queue: UnboundedReceiver<UiEvent>) -> Handle {
    Handle(oxidezap_session::spawn(async move {
        while let Some(event) = queue.recv().await {
            super::publish_one(&hub, event);
        }
    }))
}
