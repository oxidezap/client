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

use crate::session_bridge::{Action, CommandOutcome, Commands as SessionCommands, SessionCommand};
use crate::state::StateHub;

/// Build the plugin host, or an empty one when there is nowhere to look.
///
/// Failing to find a plugin directory is not a failure: the ordinary account
/// has no plugins, and a daemon that would not start without a folder is a
/// daemon that would not start.
pub fn start(hub: &Arc<StateHub>, commands: SessionCommands) -> Arc<Plugins> {
    let sink = publishing_to(hub);
    let Some(dir) = oxidezap_plugin_host::default_dir() else {
        log::debug!("no per-user data directory, so no plugins");
        return Arc::new(Plugins::none(sink));
    };
    let state_dir = oxidezap_ipc::state_dir().map(|d| d.join("plugins"));
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

impl Commands for Bridge {
    fn send_text(&self, jid: &str, text: &str, quoted: Option<&str>) -> Outcome {
        self.ask(Action::SendText {
            jid: jid.to_owned(),
            text: text.to_owned(),
            // The daemon invents one. A plugin has no bubble to rename, so a
            // local id would be a token nobody holds.
            local_id: None,
            // What a quote *shows* is the front end's business and the
            // session re-reads the original anyway, so a plugin naming the
            // message is naming everything it can honestly know.
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
