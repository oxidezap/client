//! A desktop's plugin folder, and the way a plugin acts from inside one.
//!
//! The two questions the page's half answers, answered here with a filesystem
//! and a thread pool under them: where a module comes from — a directory only
//! this user can write — and where the work runs.
//!
//! That second one is the whole of this file. Loading reads up to
//! `MAX_MODULE_BYTES` off the disk, validates it and runs its `oxi_init`,
//! none of which yields; recording an approval writes a file and renames it.
//! Both go to [`tokio::task::spawn_blocking`] rather than parking a runtime
//! worker, and both read what came back rather than dropping it — a loader
//! that panicked and was reported as having worked is the failure each of the
//! comments below is about.

use std::sync::Arc;

use oxidezap_plugin_host::{Outcome, Plugins, Reloaded, Sink};

use super::{Bridge, publishing_to};
use crate::session_bridge::{Action, CommandOutcome, Commands as SessionCommands, SessionCommand};
use crate::state::StateHub;

/// See [`super::start`].
///
/// Off the runtime's thread, for the reason in this module's header: done
/// inline it parks a runtime worker for as long as the folder takes, before
/// the daemon has bound anything. Awaited rather than detached, because the
/// session must not start until the plugins subscribed to messages are there
/// to receive them.
pub(super) async fn start(hub: &Arc<StateHub>, commands: SessionCommands) -> Arc<Plugins> {
    let sink = publishing_to(hub);
    let fallback = (publishing_to(hub), commands.clone());
    tokio::task::spawn_blocking(move || load(sink, commands))
        .await
        .unwrap_or_else(|e| {
            // With the daemon's own sink and bridge, not a discarding pair.
            // This host used to be a dead end and is not one any more: a
            // reload can put real plugins into it once whatever made the
            // loader panic has been taken out of the folder, and one built to
            // publish nowhere would run them with their interface discarded
            // and every command answering `NoSession` — while reporting the
            // reload as having worked.
            log::error!("the plugin loader did not finish: {e}");
            let (sink, commands) = fallback;
            Arc::new(Plugins::none(sink, Arc::new(Bridge { commands })))
        })
}

/// The scan itself, on the blocking thread [`start`] put it on.
fn load(sink: Sink, commands: SessionCommands) -> Arc<Plugins> {
    let Some(dir) = oxidezap_plugin_host::default_dir() else {
        log::debug!("no per-user data directory, so no plugins");
        return Arc::new(Plugins::none(sink, Arc::new(Bridge { commands })));
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

/// See [`super::reload`].
///
/// The mirror of [`start`], down to where the work happens: the scan reads
/// files and runs each `oxi_init`, all of it synchronous, so it goes to a
/// blocking thread as well.
pub(super) async fn reload(plugins: &Arc<Plugins>) -> Reloaded {
    let Some(dir) = oxidezap_plugin_host::default_dir() else {
        log::debug!("no per-user data directory, so nothing to reload");
        return Reloaded::Kept(0);
    };
    let state_dir = oxidezap_plugin_host::default_state_dir();
    let plugins = Arc::clone(plugins);
    tokio::task::spawn_blocking(move || plugins.reload_from_dir(&dir, state_dir.as_deref()))
        .await
        .unwrap_or_else(|e| {
            // Not a reload that installed nothing: a reload that did not
            // happen. The live set is whatever it was — the reservation guard
            // gives the slot back however the loader ends — and saying
            // "0 running" here put a successful-looking count directly under
            // the error.
            log::error!("the plugin loader did not finish: {e}");
            Reloaded::Failed
        })
}

/// Where [`super::reload_in_background`] puts its work: the daemon's runtime,
/// which is work-stealing, so what it is handed has to be [`Send`].
pub(super) fn detach(work: impl std::future::Future<Output = ()> + Send + 'static) {
    drop(tokio::spawn(work));
}

/// See [`super::approve`].
///
/// A write and a rename, which is disk I/O and does not belong on a runtime
/// worker. The answer is read, not dropped: a panic in there left the client
/// acknowledged for a permission the disk never received, with Settings
/// drawing a state nothing had recorded and no line in the log.
pub(super) async fn approve(plugins: &Arc<Plugins>, plugin: String, approved: bool) -> bool {
    let plugins = Arc::clone(plugins);
    match tokio::task::spawn_blocking(move || plugins.approve(&plugin, approved)).await {
        Ok(recorded) => recorded,
        Err(e) => {
            log::error!("recording a plugin approval failed: {e}");
            false
        }
    }
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
    pub(super) fn ask(&self, action: Action) -> Outcome {
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
