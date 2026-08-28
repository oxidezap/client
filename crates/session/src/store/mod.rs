//! Where the one SQLite file lives.
//!
//! Device identity, Signal state and chat history are all in it, so this is
//! the most consequential platform difference in the tree: on a desktop it is
//! a path under the user's data directory, and in a browser there is no path
//! at all. What there is instead is a VFS — SQLite's own abstraction over
//! "what a file is" — and the browser's durable one is OPFS reached through a
//! synchronous access handle.
//!
//! That handle exists in a dedicated worker and nowhere else, which is why
//! the session runs in one. The alternative VFS, IndexedDB with relaxed
//! durability, is available everywhere and is the wrong trade here: a lost
//! write to chat history is a missing message, and a lost write to Signal
//! state is a ratchet that no longer decrypts.
//!
//! [`prepare`] is the difference in one call. It has to run before anything
//! opens a connection, because installing a VFS after the fact does not move
//! a database that is already open.

#[cfg_attr(target_family = "wasm", path = "web.rs")]
#[cfg_attr(not(target_family = "wasm"), path = "native.rs")]
mod platform;

pub use platform::{database_path, prepare, wipe};

/// The database's name, wherever it is kept.
const DB_FILE: &str = "whatsapp.db";

/// Per-user data directory, under the platform data root.
///
/// No fallback to the old `whatsapp-rust-desktop` name: this app has never
/// shipped a release, so there is no installed base to migrate and carrying
/// lookup code for one would be permanent dead weight.
#[cfg_attr(target_family = "wasm", allow(dead_code))]
const DATA_DIR: &str = "oxidezap";
