//! WhatsApp client wrapper
//!
//! This module handles all communication with the WhatsApp service,
//! keeping the async/network logic separate from the UI.

mod group_notice;
mod names;
mod quoting;
mod video;
mod whatsapp;

pub use whatsapp::{ReadBoundary, WhatsAppClient, resolve_database_path, wipe_local_state};
