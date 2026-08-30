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

use oxidezap_plugin_host::{Commands, Outcome, Plugins, Sink};

#[cfg(target_family = "wasm")]
pub mod web;

#[cfg(not(target_family = "wasm"))]
use crate::session_bridge::CommandOutcome;
use crate::session_bridge::Commands as SessionCommands;
use crate::session_bridge::{Action, SessionCommand};
use crate::state::StateHub;

/// Build the plugin host, or an empty one when there is nowhere to look.
///
/// Failing to find a plugin directory is not a failure: the ordinary account
/// has no plugins, and a daemon that would not start without a folder is a
/// daemon that would not start.
pub async fn start(hub: &Arc<StateHub>, commands: SessionCommands) -> Arc<Plugins> {
    let sink = publishing_to(hub);

    // A page's plugins come out of its own origin: the modules from OPFS,
    // the approvals and each plugin's settings from `localStorage`. What is
    // *not* different is anything below this line — the same host, the same
    // sandbox, the same bounds, the same protocol carrying the surfaces to
    // whatever is drawing them. What a page gives a plugin instead of a
    // thread is a task on its own loop; see `oxidezap_plugin_host::sched`.
    //
    // Awaited rather than spawned, for the binary's reason: the session must
    // not start until the plugins subscribed to messages are there to receive
    // them.
    #[cfg(target_family = "wasm")]
    {
        let modules = web::installed().await;
        Arc::new(Plugins::start(
            modules,
            Arc::new(oxidezap_plugin_host::Origin::storage()),
            Arc::new(Bridge { commands }),
            sink,
        ))
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
struct Bridge {
    commands: SessionCommands,
}

impl Bridge {
    /// Hand one action to the session and wait for what it made of it.
    ///
    /// Blocking, on the plugin's own thread, and that is the point: the
    /// answer *is* what the plugin gets out of the call, and a queue would
    /// hand it back the same "it was taken" a socket front end already has to
    /// live with. Nothing on the daemon's side waits for this — the plugin
    /// thread is the only one parked — and a plugin parked here is one whose
    /// own queue is filling, which the host already has a rule for.
    #[cfg(not(target_family = "wasm"))]
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

    /// Hand one action to the session, without waiting for what it made of
    /// it.
    ///
    /// The one place a page's plugin is weaker than a desktop's, and it is
    /// not a shortcut: the plugin's call is synchronous wasm on the *same*
    /// agent the bridge runs on, so waiting for the answer would be waiting
    /// for a task that cannot run until this call returns — a deadlock, not a
    /// delay. So a page's plugin gets the same "it was taken" a socket front
    /// end already lives with.
    ///
    /// What is still honest here is the refusal: a full command channel is a
    /// session that will not take this now, and a closed one is no session at
    /// all. Both are the answers a plugin acts on; only `Refused` for a
    /// command the daemon would have declined is lost, and that arrives in
    /// the event stream as it does for every other front end.
    #[cfg(target_family = "wasm")]
    fn ask(&self, action: Action) -> Outcome {
        use tokio::sync::mpsc::error::TrySendError;

        // Dropped, not awaited. The command is answered on a channel nobody
        // is listening to, which the bridge already tolerates: every other
        // sender there is a connection that has gone.
        let (reply, _answer) = tokio::sync::oneshot::channel();
        match self.commands.try_send(SessionCommand { action, reply }) {
            Ok(()) => Outcome::Accepted,
            Err(TrySendError::Full(_)) => Outcome::Refused,
            Err(TrySendError::Closed(_)) => Outcome::NoSession,
        }
    }
}

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
