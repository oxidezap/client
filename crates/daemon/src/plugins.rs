//! Wiring the plugin host into the daemon.
//!
//! Two directions, and each is a small adapter rather than a mechanism. What
//! a plugin does goes onto the same command channel a front end's requests
//! go onto — a plugin is a front end that does not draw, so it has no
//! privileged path to the session. What a plugin publishes goes into
//! [`StateHub`] as ordinary versioned state, which is what makes a plugin's
//! interface survive a window closing and reappear in the next window's
//! snapshot.

use std::sync::Arc;

#[cfg(not(target_family = "wasm"))]
use oxidezap_plugin_host::{Commands, Outcome};
use oxidezap_plugin_host::{Plugins, Sink};

use crate::session_bridge::Commands as SessionCommands;
#[cfg(not(target_family = "wasm"))]
use crate::session_bridge::{Action, CommandOutcome, SessionCommand};
use crate::state::StateHub;

/// Build the plugin host, or an empty one when there is nowhere to look.
///
/// Failing to find a plugin directory is not a failure: the ordinary account
/// has no plugins, and a daemon that would not start without a folder is a
/// daemon that would not start.
pub async fn start(hub: &Arc<StateHub>, commands: SessionCommands) -> Arc<Plugins> {
    let sink = publishing_to(hub);

    // A page runs no plugins, and this is where that is decided rather than
    // where it would otherwise happen by accident. Two things are missing and
    // only one of them is a folder: a plugin gets its own OS thread and its
    // own wasmi `Store`, and a tab has no threads to give it — the same fact
    // r2d2 ran into, and the reason `Plugins::load` would publish a set of
    // entries every one of which reads "its thread could not be started".
    // There is nowhere to discover a module from either, and nowhere to keep
    // an approval so the answer survives a reload.
    //
    // Left as an early return with a reason instead of relying on
    // `default_dir` answering `None`, which it does here only because a
    // browser has no `HOME`: a refusal that depends on an environment
    // variable being absent is one that comes back the moment somebody sets
    // it. What the *front end* draws in place of a plugin list is its own
    // (`platform::plugins_unavailable`); this is the daemon half, and the two
    // have to agree.
    #[cfg(target_family = "wasm")]
    {
        let _ = commands;
        log::info!("plugins need a daemon with threads and a filesystem; this page has neither");
        return Arc::new(Plugins::none(sink));
    }

    #[cfg(not(target_family = "wasm"))]
    {
        // Off the runtime's thread. Loading reads up to `MAX_MODULE_BYTES` a
        // module off the disk, validates it and runs its `oxi_init`, all of
        // it synchronous — done here it parks a runtime worker for as long as
        // the folder takes, before the daemon has bound anything. Awaited
        // rather than detached, because the session must not start until the
        // plugins subscribed to messages are there to receive them.
        tokio::task::spawn_blocking(move || start_here(sink, commands))
            .await
            .unwrap_or_else(|e| {
                log::error!("the plugin loader did not finish: {e}");
                Arc::new(Plugins::none(publishing_to_nothing()))
            })
    }
}

/// A sink for a set of plugins that will never publish anything.
#[cfg(not(target_family = "wasm"))]
fn publishing_to_nothing() -> Sink {
    Arc::new(|_| {})
}

/// The half that needs a filesystem.
#[cfg(not(target_family = "wasm"))]
fn start_here(sink: Sink, commands: SessionCommands) -> Arc<Plugins> {
    let Some(dir) = oxidezap_plugin_host::default_dir() else {
        log::debug!("no per-user data directory, so no plugins");
        return Arc::new(Plugins::none(sink));
    };
    // Not the daemon's `state_dir`: that one prefers XDG_RUNTIME_DIR, which
    // is cleared on logout, and a permission answer that does not survive a
    // logout is a prompt asked forever.
    let state_dir = oxidezap_plugin_host::default_state_dir();
    Arc::new(Plugins::load(
        &dir,
        state_dir.as_deref(),
        Arc::new(Bridge { commands }),
        sink,
    ))
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
/// Native only, because it is the thing a plugin thread calls into and a page
/// starts no plugin threads. Left out rather than compiled and unused, so
/// `blocking_send` — which a browser's single agent may not make — is not
/// reachable from a page's build at all.
#[cfg(not(target_family = "wasm"))]
struct Bridge {
    commands: SessionCommands,
}

#[cfg(not(target_family = "wasm"))]
impl Bridge {
    /// Hand one action to the session and wait for what it made of it.
    ///
    /// Blocking, on the plugin's own thread, and that is the point: the
    /// answer *is* what the plugin gets out of the call, and a queue would
    /// hand it back the same "it was taken" a socket front end already has to
    /// live with. Nothing on the daemon's side waits for this — the plugin
    /// thread is the only one parked — and a plugin parked here is one whose
    /// own queue is filling, which the host already has a rule for.
    fn ask(&self, action: Action) -> Outcome {
        let (reply, answer) = tokio::sync::oneshot::channel();
        if self
            .commands
            .blocking_send(SessionCommand { action, reply })
            .is_err()
        {
            // The bridge is gone: the daemon is shutting down, which is not a
            // refusal of this particular command.
            return Outcome::NoSession;
        }
        match answer.blocking_recv() {
            Ok(CommandOutcome::Accepted) => Outcome::Accepted,
            Ok(CommandOutcome::NoSession(_)) | Err(_) => Outcome::NoSession,
            Ok(CommandOutcome::Refused(_)) => Outcome::Refused,
        }
    }
}

#[cfg(not(target_family = "wasm"))]
impl Commands for Bridge {
    fn send_text(&self, jid: &str, text: &str, quoted: Option<&str>) -> Outcome {
        self.ask(Action::SendText {
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
            // note in AGENTS.md under "Still to do".
            quoted: quoted.map(|id| oxidezap_core::QuotedMessage {
                message_id: id.to_owned(),
                sender: String::new(),
                sender_name: String::new(),
                preview: String::new(),
                kind: None,
            }),
        })
    }

    fn mark_read(&self, jid: &str, message_id: Option<&str>) -> Outcome {
        self.ask(Action::MarkRead {
            jid: jid.to_owned(),
            through_message_id: message_id.map(str::to_owned),
        })
    }

    fn typing(&self, jid: &str, composing: bool) -> Outcome {
        self.ask(Action::Typing {
            jid: jid.to_owned(),
            composing,
        })
    }
}
