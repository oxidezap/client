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

use oxidezap_plugin_host::{Commands, Outcome, Plugins, Reloaded, Sink};

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
        Arc::new(
            Plugins::start(
                modules,
                Arc::new(oxidezap_plugin_host::Origin::storage()),
                Arc::new(Bridge { commands }),
                sink,
            )
            .await,
        )
    }

    #[cfg(not(target_family = "wasm"))]
    {
        // Off the runtime's thread. Loading reads up to `MAX_MODULE_BYTES` a
        // module off the disk, validates it and runs its `oxi_init`, all of
        // it synchronous — done here it parks a runtime worker for as long as
        // the folder takes, before the daemon has bound anything. Awaited
        // rather than detached, because the session must not start until the
        // plugins subscribed to messages are there to receive them.
        let fallback = (publishing_to(hub), commands.clone());
        tokio::task::spawn_blocking(move || start_here(sink, commands))
            .await
            .unwrap_or_else(|e| {
                // With the daemon's own sink and bridge, not a discarding
                // pair. This host used to be a dead end and is not one any
                // more: a reload can put real plugins into it once whatever
                // made the loader panic has been taken out of the folder, and
                // one built to publish nowhere would run them with their
                // interface discarded and every command answering
                // `NoSession` — while reporting the reload as having worked.
                log::error!("the plugin loader did not finish: {e}");
                let (sink, commands) = fallback;
                Arc::new(Plugins::none(sink, Arc::new(Bridge { commands })))
            })
    }
}

/// Read the plugin folder again and replace what is running with what is in
/// it now, without stopping the daemon or the session.
///
/// The mirror of [`start`], down to where the work happens: a desktop's scan
/// reads files and runs each `oxi_init`, all of it synchronous, so it goes to
/// a blocking thread; a page's modules come out of OPFS and its host runs on
/// the page's own loop, so it is awaited here.
///
/// Answers what the reload did, rather than a count: three of the four
/// outcomes are zero plugins installed and mean different things, and the
/// count is what gets written to the log.
pub async fn reload(plugins: &Arc<Plugins>) -> Reloaded {
    #[cfg(target_family = "wasm")]
    {
        // Handed over as a future rather than as values, and that is not
        // style: `Origin::storage()` *stamps* the origin's storage, retiring
        // every handle taken before it. `Plugins::reload` refuses a second
        // reload while one is running, so gathering these eagerly would let a
        // refused call retire the handle the surviving generation is about to
        // be installed with — every approval and settings write refused
        // afterwards, and a revoked grant left on disk to come back. A future
        // does nothing until it is polled, which is after the reservation.
        let host = Arc::clone(plugins);
        let plugins = &host;
        plugins
            .reload(|| async {
                // `discover` and not `installed`: the fallible one. A folder
                // that cannot be read is not an empty folder, and treating it
                // as one here would retire every healthy plugin and publish
                // an empty set over a transient storage error.
                let modules = web::discover().await.ok()?;
                // And the host is still this account's. `ForgetSession` can
                // land while that await is suspended, and a page rebuilds its
                // whole service in the same agent — so by the time this
                // resumes, a *replacement* host may already hold the newest
                // storage handle. Taking one here would retire it, and every
                // approval and settings write the new host makes would be
                // refused until some later reload happened to succeed.
                // `reload` rechecks too, but only after the stamp has moved,
                // which is exactly too late.
                if plugins.is_retired() {
                    return None;
                }
                // And the fresh handle only once there is something to
                // install with it: taking one retires every older handle, so
                // a scan that failed would leave the running generation
                // writing through a store it no longer owns.
                let state: Arc<dyn oxidezap_plugin_host::Backing> =
                    Arc::new(oxidezap_plugin_host::Origin::storage());
                Some((modules, state))
            })
            .await
    }

    #[cfg(not(target_family = "wasm"))]
    {
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
                // happen. The live set is whatever it was — the reservation
                // guard gives the slot back however the loader ends — and
                // saying "0 running" here put a successful-looking count
                // directly under the error.
                log::error!("the plugin loader did not finish: {e}");
                Reloaded::Failed
            })
    }
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
/// Two spawns rather than one, for the reason every split in this module
/// exists: a page's tasks are not `Send` and there is no runtime to hand one
/// to, so it goes on the loop it is already running on.
pub fn reload_in_background(plugins: &Arc<Plugins>) {
    let plugins = Arc::clone(plugins);
    let work = async move {
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
    };

    #[cfg(target_family = "wasm")]
    wasm_bindgen_futures::spawn_local(work);

    #[cfg(not(target_family = "wasm"))]
    drop(tokio::spawn(work));
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
    #[cfg(target_family = "wasm")]
    {
        // Inline, and there is nowhere else it could go. `spawn_blocking`
        // needs a blocking pool, and a page's runtime has none — nor could
        // it, since a browser agent is one thread. What this costs is the
        // write itself, which is a `localStorage` set: the same call a
        // plugin's own settings already make from inside a wasm call.
        plugins.approve(&plugin, approved)
    }

    #[cfg(not(target_family = "wasm"))]
    {
        let plugins = Arc::clone(plugins);
        // The answer is read, not dropped: a panic in there left the client
        // acknowledged for a permission the disk never received, with
        // Settings drawing a state nothing had recorded and no line in the
        // log.
        match tokio::task::spawn_blocking(move || plugins.approve(&plugin, approved)).await {
            Ok(recorded) => recorded,
            Err(e) => {
                log::error!("recording a plugin approval failed: {e}");
                false
            }
        }
    }
}

/// The half that needs a filesystem.
#[cfg(not(target_family = "wasm"))]
fn start_here(sink: Sink, commands: SessionCommands) -> Arc<Plugins> {
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
            // note in docs/roadmap.md.
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
