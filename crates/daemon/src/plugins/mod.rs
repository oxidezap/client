//! Wiring the plugin host into the daemon.
//!
//! Two directions, and each is a small adapter rather than a mechanism. What
//! a plugin does goes onto the same command channel a front end's requests
//! go onto — a plugin is a front end that does not draw, so it has no
//! privileged path to the session. What a plugin publishes goes into
//! [`StateHub`] as ordinary versioned state, which is what makes a plugin's
//! interface survive a window closing and reappear in the next window's
//! snapshot.
//!
//! What differs by platform is neither of those directions: it is *where a
//! module comes from* and *where the work runs*. A desktop reads a directory
//! only this user can write, and every scan and every approval is synchronous
//! I/O that has to leave the runtime's threads. A page reads OPFS, keeps its
//! approvals in `localStorage`, and has no blocking pool to move anything to
//! — nor could it, since a browser agent is one thread. So the two live in
//! `native` and `web` as the same four functions plus `Bridge::ask`, and
//! everything in this file is written once. What is *not* different is
//! anything below that line: the same host, the same sandbox, the same
//! bounds, the same protocol carrying the surfaces to whatever is drawing
//! them.

use std::sync::Arc;

use oxidezap_plugin_host::{Commands, Outcome, Plugins, Reloaded, Sink};

#[cfg(not(target_family = "wasm"))]
mod native;
/// Public where its sibling is private, and that is the one asymmetry: a
/// desktop's plugin folder is the operating system's to fill, and a page's is
/// this module's, so installing, listing and removing are calls a wasm front
/// end makes.
#[cfg(target_family = "wasm")]
pub mod web;

// Named once so nothing below has to ask which one it is. Two `mod` items
// rather than the `#[cfg_attr(path)]` idiom used elsewhere in the tree,
// because `web` has a submodule — the browser tests — and rustfmt resolves a
// `#[path]` module's children against the wrong directory.
#[cfg(not(target_family = "wasm"))]
use native as platform;
#[cfg(target_family = "wasm")]
use web as platform;

use crate::session_bridge::{Action, Commands as SessionCommands};
use crate::state::StateHub;

/// Build the plugin host, or an empty one when there is nowhere to look.
///
/// Failing to find a plugin directory is not a failure: the ordinary account
/// has no plugins, and a daemon that would not start without a folder is a
/// daemon that would not start.
///
/// Awaited rather than spawned on either platform, for the binary's reason:
/// the session must not start until the plugins subscribed to messages are
/// there to receive them.
pub async fn start(hub: &Arc<StateHub>, commands: SessionCommands) -> Arc<Plugins> {
    platform::start(hub, commands).await
}

/// Read the plugin folder again and replace what is running with what is in
/// it now, without stopping the daemon or the session.
///
/// The mirror of [`start`], down to where the work happens: a desktop's scan
/// reads files and runs each `oxi_init`, all of it synchronous, so it goes to
/// a blocking thread; a page's modules come out of OPFS and its host runs on
/// the page's own loop, so it is awaited there.
///
/// Answers what the reload did, rather than a count: three of the four
/// outcomes are zero plugins installed and mean different things, and the
/// count is what gets written to the log.
pub async fn reload(plugins: &Arc<Plugins>) -> Reloaded {
    platform::reload(plugins).await
}

/// The same, off the caller's own task.
///
/// What the IPC server asks for, because the caller there is one connection's
/// loop: awaiting a reload in it is a window served nothing for as long as the
/// folder takes — no state, no session events, and no call video, which is
/// eight frames deep and overflows in a fraction of a second. Nothing is
/// waiting for the answer either; what came back is state, and every front end
/// reads it in the same frame.
///
/// Where the work goes is the platform's, for the reason every split in this
/// module exists: a page's tasks are not `Send` and there is no runtime to
/// hand one to, so it goes on the loop it is already running on.
pub fn reload_in_background(plugins: &Arc<Plugins>) {
    let plugins = Arc::clone(plugins);
    platform::detach(async move {
        // Said as what it was. A deferred pass and a loader that fell over
        // both installed nothing, and both used to be reported as a reload
        // that finished with none running — over a folder of five healthy
        // plugins, in the first case, all of them still going.
        match reload(&plugins).await {
            Reloaded::Ran(running) => log::info!("plugins reloaded: {running} running"),
            Reloaded::Deferred => {
                log::info!("a plugin reload is already running; it will cover this one");
            }
            Reloaded::Kept(running) => {
                log::warn!("plugins not reloaded; the {running} that were running still are");
            }
            Reloaded::Failed => log::warn!("plugins were not reloaded"),
        }
    });
}

/// Record what somebody answered about a plugin's permissions.
///
/// A platform split for one reason, and it is the reason every other one here
/// exists: a desktop writes and renames a file, which is disk I/O that must
/// not run on a runtime worker, and a page writes `localStorage`, which is
/// synchronous by construction and has no blocking pool to be moved to. This
/// was `spawn_blocking` on both, and on a page that is not a slow answer but
/// a panic — "there is no reactor running" — so approving a plugin in the
/// browser has never once worked.
///
/// Answers whether it was recorded, and the caller refuses the request rather
/// than acknowledging a permission nothing holds. Two ways to answer `false`,
/// and neither used to be said: the store refusing the write — a quota, a
/// browsing context with no `localStorage`, a disk — where a *grant* is then
/// rolled back and the plugin is left unapproved, and the thread that was
/// writing it having panicked.
pub async fn approve(plugins: &Arc<Plugins>, plugin: String, approved: bool) -> bool {
    platform::approve(plugins, plugin, approved).await
}

/// Where a plugin's published interface goes.
///
/// Through [`StateHub::set_plugins`], which is to say through the same
/// versioned channel every other piece of daemon state travels on. Called
/// from a plugin's own thread, which the hub's lock already accounts for.
fn publishing_to(hub: &Arc<StateHub>) -> Sink {
    let hub = Arc::clone(hub);
    Arc::new(move |surfaces| hub.set_plugins(surfaces))
}

/// The plugin host's view of the session.
///
/// [`Bridge::ask`] is the platform's — a desktop's plugin runs on a thread of
/// its own and can wait for the answer, a page's cannot — so it lives beside
/// the rest of the split. Everything a plugin can actually ask for is here,
/// written once on top of it.
struct Bridge {
    commands: SessionCommands,
}

impl Commands for Bridge {
    fn send_text(&self, jid: &str, text: &str, quoted: Option<&str>) -> Outcome {
        self.ask(Action::SendText(oxidezap_ipc::SendText {
            jid: jid.to_owned(),
            text: text.to_owned(),
            // The daemon invents one. A plugin has no bubble to rename, so a
            // local id would be a token nobody holds.
            local_id: None,
            // A plugin knows the id and nothing else, which is all the ABI
            // gives it. The session does *not* re-read the original —
            // `quote_context` serializes these fields straight onto the wire —
            // so the quote bar the peer sees carries the reply's linkage and
            // an empty body, and in a group it names no author. Filling that
            // in means a lookup the daemon has no store to make; see the
            // note in docs/roadmap.md.
            quoted: quoted.map(|id| oxidezap_core::QuotedMessage {
                message_id: id.to_owned(),
                sender: String::new(),
                sender_name: String::new(),
                preview: String::new(),
                kind: None,
            }),
        }))
    }

    fn mark_read(&self, jid: &str, message_id: Option<&str>) -> Outcome {
        self.ask(Action::MarkRead(oxidezap_ipc::MarkRead {
            jid: jid.to_owned(),
            through_message_id: message_id.map(str::to_owned),
        }))
    }

    fn typing(&self, jid: &str, composing: bool) -> Outcome {
        self.ask(Action::Typing(oxidezap_ipc::Typing {
            jid: jid.to_owned(),
            composing,
        }))
    }
}
