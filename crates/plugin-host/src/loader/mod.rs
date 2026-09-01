//! Where a plugin's modules come from.
//!
//! A desktop finds them: a folder is scanned in name order, each `.wasm` in
//! it is a plugin id, and the mode and owner of both the folder and the file
//! are what make an approval recorded against that id worth anything. A page
//! is handed them — there is no directory to scan, no file to open and no
//! mode to read — so its modules arrive through
//! [`Plugins::start`](crate::Plugins::start), found by the daemon's own
//! `plugins::web` in the origin's storage.
//!
//! Which is the same shape as `sched` beside it, and as the session's `exec`
//! before that: one path attribute, two files, and no `cfg` in the host above
//! them. What is different here is that the two halves are not the same list
//! of names. The page's is one function — the wait for a load that is
//! part-way through — and it gets no stub of the rest: a `discover` that
//! always answered "none" would be a directory scan a page could call, and
//! the honest answer is that the name does not exist there. So the crate root
//! is where the desktop's names are spelled out, and where each gets back the
//! visibility it had before the move.

#[cfg_attr(target_family = "wasm", path = "web.rs")]
#[cfg_attr(not(target_family = "wasm"), path = "native.rs")]
mod platform;

// Whatever the half compiled here has, which is deliberately not the same
// list on both: the page's is one function and the desktop's is a filesystem.
// Naming them here would mean a `cfg` per platform above a `cfg` that has
// already chosen one — and the crate root, which decides what is public and
// what is only the store's and the tests', names them anyway. That
// `Plugins::load` and `Plugins::reload_from_dir` are not in either list is
// the same idea: they are inherent methods on the host, so a desktop's host
// is the same host with a folder behind it and nothing has to be re-exported
// for it.
pub use platform::*;
