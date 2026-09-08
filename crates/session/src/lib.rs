//! WhatsApp client wrapper
//!
//! This module handles all communication with the WhatsApp service,
//! keeping the async/network logic separate from the UI.

mod exec;
mod group_notice;
mod names;
mod net;
mod quoting;
mod relay;
mod store;
mod video;
mod whatsapp;

#[cfg(all(feature = "test-support", not(target_family = "wasm")))]
pub use whatsapp::{OutgoingAcceptCase, outgoing_accept_events};

pub use exec::{Cancelled, Task, sleep, spawn, unblock, with_timeout};
pub use whatsapp::{
    OutgoingFile, ReadBoundary, UiEventReceiver, UiEventTryRecvError, WhatsAppClient,
    prepare_store, resolve_database_path, wipe_local_state,
};
