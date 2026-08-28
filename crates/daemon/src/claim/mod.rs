//! One session per user, enforced where the copies can appear.
//!
//! The binary answers this with a file lock taken before anything touches the
//! account: a second `oxidezapd` fails fast rather than racing the first over
//! one SQLite file. A page has the same problem in a different shape — two
//! tabs on one origin are two of everything, and nothing about opening a
//! second tab tells the first — and the browser has the same answer, which is
//! a lock held for as long as the holder is alive.
//!
//! What is being protected is not tidiness. Both tabs would preload the same
//! database into memory, write it back independently, and advance the same
//! Signal state from two places: the losing writer's chats disappear and its
//! ratchets stop decrypting.

#[cfg_attr(target_family = "wasm", path = "web.rs")]
#[cfg_attr(not(target_family = "wasm"), path = "native.rs")]
mod platform;

pub(crate) use platform::take;
