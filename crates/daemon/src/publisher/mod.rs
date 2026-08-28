//! Turning session events into frames, off the loop that produced them.
//!
//! One event becomes one frame, and making it costs two things worth keeping
//! away from the event loop: the media is written out of the event and into
//! wherever media lives, and the rest is serialized. The queue in front of
//! this is unbounded because the only producer is that loop, and a bound
//! could only stall the thing this exists to unblock.
//!
//! *Where* the draining happens is the platform question. A daemon has a
//! thread to give it, and wants one: the media write is file I/O, and doing
//! that on a runtime worker blocks everything else the runtime is driving. A
//! page has no thread to give and does not need one — its media write is a
//! map insert — so the drain is a task on the loop it already turns.
//!
//! What both owe the caller is the same, and it is not tidiness: the
//! publisher writes this account's media, so a wipe that starts while it is
//! still draining deletes a directory about to be written into again. The
//! queue is closed and the drain is *waited for*, which is what [`Handle`] is
//! for.

#[cfg_attr(target_family = "wasm", path = "web.rs")]
#[cfg_attr(not(target_family = "wasm"), path = "native.rs")]
mod platform;

pub(crate) use platform::{Handle, start};

use oxidezap_core::UiEvent;
use oxidezap_ipc::DaemonMessage;

use crate::state::StateHub;

/// Publish one event, whichever side of the split is draining.
fn publish_one(hub: &StateHub, mut event: UiEvent) {
    super::session_bridge::externalize_media(&mut event);
    match serde_json::to_string(&DaemonMessage::Session {
        event: Box::new(event),
    }) {
        Ok(frame) => hub.publish_session(frame),
        Err(e) => log::error!("dropping unserializable session event: {e}"),
    }
}
