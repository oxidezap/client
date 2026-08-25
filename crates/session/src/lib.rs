//! WhatsApp client wrapper
//!
//! This module handles all communication with the WhatsApp service,
//! keeping the async/network logic separate from the UI.

mod quoting;
mod whatsapp;

pub use whatsapp::{ReadBoundary, WhatsAppClient, wipe_local_state};
