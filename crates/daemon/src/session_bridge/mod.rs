//! Translates the session's `UiEvent` stream into daemon state, and carries
//! client commands the other way.
//!
//! The only writer to [`StateHub`]. Everything else observes, which is what
//! makes "one owner" more than a convention. Commands arrive on a channel
//! rather than through a shared handle for the same reason: the session is
//! touched from exactly one task, so a send and the state it produces cannot
//! interleave with anything else.
//!
//! Split by what each part is about rather than by layer, because the run
//! loop below is the only thing that knows about all of them: [`action`] is
//! what a client may ask for, [`translate`] is the event stream becoming
//! state, [`act`] is a command becoming work, [`read_tracker`] is the unread
//! model those two share, and [`externalize`] is where a frame's media bytes
//! go. The `Bridge` itself stays here, with the loop that drives it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use oxidezap_core::UiEvent;
use oxidezap_session::{StoreRegistry, WhatsAppClient};
use tokio::sync::Semaphore;

use crate::state::StateHub;

mod act;
mod action;
mod externalize;
mod read_tracker;
mod translate;

#[cfg(test)]
mod tests;

pub use action::{
    AccountDisposition, AccountExit, Action, CommandOutcome, Commands, Outbox, SessionCommand,
};
pub(crate) use externalize::externalize_media;

use act::MAX_IN_FLIGHT;
use read_tracker::ReadTracker;
use translate::Answer;

/// Lifecycle bits owned by one account runtime.
///
/// This deliberately is not process-global: resetting account A must not make
/// account B refuse a new connection or appear to be stopping.
#[derive(Clone, Debug)]
pub struct RuntimeLifecycle {
    stopping: Arc<AtomicBool>,
    /// Set once, by whichever [`Action::ForgetSession`] stops this runtime.
    /// Read after `run()` returns by the supervisor that spawned it, which
    /// decides from this alone whether to respawn the id (`Reset`), drop it
    /// for good (`Remove`), or do neither — a session that ended on its own,
    /// or a process-wide shutdown, leaves this `None`.
    disposition: Arc<Mutex<Option<AccountDisposition>>>,
}

impl RuntimeLifecycle {
    #[must_use]
    pub fn new() -> Self {
        Self {
            stopping: Arc::new(AtomicBool::new(false)),
            disposition: Arc::new(Mutex::new(None)),
        }
    }

    pub fn mark_stopping(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Record why this runtime is stopping, and mark it stopping at the same
    /// time — the two always go together, so there is nowhere to set one
    /// without the other.
    ///
    /// The first call wins, and answers what this call was relative to it, so
    /// a caller can tell a request it owns from one it lost. Silently keeping
    /// the first was enough while the only caller drew no distinction; with
    /// two control requests able to arrive together (`ResetAccount` then
    /// `RemoveAccount` on the same id) a second, incompatible request used to
    /// be answered `Accepted` while the first was the only one that would
    /// ever run. See [`DispositionOutcome`].
    #[must_use]
    pub fn set_disposition(&self, disposition: AccountDisposition) -> DispositionOutcome {
        let outcome = {
            let mut slot = self
                .disposition
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match *slot {
                None => {
                    *slot = Some(disposition);
                    DispositionOutcome::Recorded
                }
                Some(held) if held == disposition => DispositionOutcome::AlreadySame,
                Some(_) => DispositionOutcome::Conflict,
            }
        };
        self.mark_stopping();
        outcome
    }

    /// What this runtime's teardown should do to storage, if anything was
    /// asked before it stopped.
    #[must_use]
    pub fn disposition(&self) -> Option<AccountDisposition> {
        *self
            .disposition
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for RuntimeLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

/// What a [`RuntimeLifecycle::set_disposition`] call was, relative to whatever
/// reason the runtime was already stopping for.
///
/// The runtime stops once, for one reason. A caller whose request caused that
/// stop owns the outcome; a caller whose request arrived after a different one
/// did not, and must be told so rather than left believing its operation is
/// the one in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispositionOutcome {
    /// Nothing was stopping this runtime; this call is what will run.
    Recorded,
    /// The runtime was already stopping for this same reason. A second
    /// identical ask is not a conflict: it is the same operation, and the
    /// caller may still report it as accepted.
    AlreadySame,
    /// The runtime was already stopping for a different reason. This request
    /// will not run — the first one wins.
    Conflict,
}

/// Run the session until it ends or `shutdown` resolves.
///
/// Shutdown is a parameter rather than something the caller races this future
/// against: losing a `select!` would drop this future mid-await, and the
/// session would be torn down by `Drop` with nobody waiting for its thread to
/// disconnect and close SQLite. Owning the signal is what makes the teardown
/// below reachable on every exit path.
///
/// Returns the [`AccountExit`] the teardown actually produced, never the
/// [`AccountDisposition`] that was merely asked for — see that type's own
/// doc for why a caller must not conflate the two.
pub async fn run(
    account_id: oxidezap_core::AccountId,
    stores: Arc<StoreRegistry>,
    hub: Arc<StateHub>,
    plugins: Arc<oxidezap_plugin_host::Plugins>,
    mut commands: tokio::sync::mpsc::Receiver<SessionCommand>,
    lifecycle: RuntimeLifecycle,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<AccountExit> {
    debug_assert_eq!(hub.account_id(), account_id);
    // Kept alive past the client's own close: the teardown below calls back
    // into the registry to reset this account's rows, which needs a handle
    // of its own rather than the one the client just gave up.
    let stores_for_reset = Arc::clone(&stores);
    let mut client = WhatsAppClient::new_for_account_with_registry(account_id, stores)
        .context("opening the local store")?;
    let mut events = client
        .start()
        .map_err(|e| anyhow::anyhow!("starting the session: {e}"))?;
    // Asked for once, here, rather than per front end: the session has one
    // camera and one call, and what decides whether a frame is *serialized*
    // is whether anybody is subscribed to the hub's video channel.
    let mut video = client.video_events();
    let mut bridge = Bridge::with_lifecycle(hub, plugins, lifecycle);

    // Set when every sender is gone. A closed channel yields `None`
    // immediately and forever, so leaving the branch enabled would spin the
    // loop at full speed instead of waiting for events.
    let mut commands_closed = false;
    // What ended the loop. Carried out of it so the teardown can classify the
    // outcome: only the two `break`s that are "the daemon is stopping" produce
    // `AccountExit::Stopped`; a session that ended while the process is still
    // running is a fault worth at least reporting as recoverable.
    let mut stopping = false;

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => {
                    if let Answer::Decline(call_id) = bridge.observe(event) {
                        client.decline_call(&call_id);
                    }
                }
                // The session dropped its sender: the run loop is gone and no
                // further event can arrive.
                None => break,
            },
            // Not folded into daemon state and not published as an event: a
            // frame is neither. It goes straight out to whoever is drawing —
            // and when nobody is, the session is told to stop producing them
            // rather than being left to hand over frames this drops. That is
            // the only place the *last* window leaving can be noticed:
            // nothing announces a subscriber going away, and one frame is
            // what it costs to find out.
            //
            // Offered, then answered. Asking first and publishing second was
            // two questions with a gap between them: the reader could leave in
            // it, and the frame was then dropped by the publish while this
            // side, having been told there was a reader, left the camera
            // running for another one.
            Some(frame) = video.recv() => {
                if bridge.hub.publish_video(frame).is_unwanted() {
                    client.set_video_publishing(false);
                }
            }

            command = commands.recv(), if !commands_closed => match command {
                Some(command) => {
                    bridge.execute(&client, command).await;
                    // Asked to stop and be reset or removed: stop here so the
                    // teardown below runs before anything touches the row
                    // it is closing.
                    if bridge.lifecycle.disposition().is_some() {
                        break;
                    }
                }
                None => commands_closed = true,
            },
            () = &mut shutdown => {
                stopping = true;
                break;
            }
        }
    }

    // Read once and carried by value from here on: nothing after this can
    // change it (the command channel is about to be dropped), and a `Copy`
    // enum is simpler to match on than a method call at every site that used
    // to read `bridge.forget`.
    let disposition = bridge.lifecycle.disposition();

    // Reached whether the session ended on its own or a signal arrived.
    //
    // Both of the things that would panic here — a join that blocks and the
    // drop of a tokio runtime inside an async context — belong to the client
    // rather than to this loop, so it does them: see `WhatsAppClient::close`.
    let grace = if disposition.is_some() {
        FORGET_GRACE
    } else {
        SHUTDOWN_GRACE
    };
    let closed = client.close(grace).await;

    // Before the plugins are joined, and this ordering is the whole of it: a
    // plugin thread that issued a command is parked on its answer, and the
    // loop that would have answered has just stopped running. Dropping the
    // receiver ends both halves at once — a command already queued has its
    // reply channel dropped with it, so the plugin's wait returns, and every
    // command after this fails to send at all. Joining first would have the
    // teardown waiting for a thread waiting for the teardown.
    drop(commands);

    // Plugins next, and for exactly the reason the publisher is joined
    // below: one still in a handler can write its settings file, and that
    // file sits in a directory the wipe is about to remove.
    //
    // Through `unblock` rather than `spawn_blocking`, because this line is
    // reached in a page too: a browser has no blocking pool, so the call
    // that was meant to join threads would instead panic here — before the
    // publisher is joined and before the store is deleted, which is the
    // whole of what this teardown exists to order. `unblock` is a hand-off
    // on a desktop and a plain call in a page, which is right on both: a
    // page's plugins are tasks on this very loop, so there is nothing to
    // join and nothing that could be running while this runs. What their
    // *last* write cannot be ordered against is the retirement below, which
    // is why the origin's storage stamps the account a store was opened for
    // and refuses a write from an older one — see `plugin_host::Origin`. A
    // page can pair again without reloading, so what is refused has to be the
    // departed account's handles rather than every handle from here on.
    {
        let plugins = Arc::clone(&bridge.plugins);
        if oxidezap_session::unblock(move || plugins.shutdown())
            .await
            .is_err()
        {
            log::error!("the plugin threads did not finish");
        }
    }

    // Before anything is deleted, and on a blocking thread because joining
    // one is: the publisher writes this account's media, and a wipe that
    // starts while it is still draining its queue deletes a directory that
    // is about to be written into again.
    if let Some(publisher) = bridge.stop_publishing() {
        publisher.join().await;
    }

    /// Whether the record of what the user allowed each plugin is gone.
    ///
    /// `true` when there was nothing to remove, which is the ordinary case: an
    /// account with no plugins has no permissions to retire.
    #[cfg_attr(target_family = "wasm", allow(unused_variables))]
    fn approvals_retired(account_id: oxidezap_core::AccountId) -> bool {
        // A page keeps them in its origin's storage rather than in a
        // directory, and clears the plugins' settings in the same sweep:
        // there is no directory below to remove afterwards, so the two halves
        // that are separate on a desktop are one call here. What survives is
        // what survives there — the modules themselves.
        //
        // Not yet account-scoped there (the multi-account plan's section
        // 13.3 names this as pending), so this account's id goes unused on
        // that half of the split.
        #[cfg(target_family = "wasm")]
        {
            oxidezap_plugin_host::Origin::forget_all()
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let Some(dir) = crate::plugins::account_state_dir(account_id) else {
                return true;
            };
            match oxidezap_plugin_host::forget_approvals(&dir) {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Err(e) => {
                    log::error!("cannot remove the plugins' recorded permissions: {e}");
                    false
                }
            }
        }
    }

    // After the teardown, never before: `reset_account` purges this account's
    // rows in the *shared* database, and WR-1's own caller obligation is that
    // nothing still holding a handle bound to this account's id may be live
    // when that runs — a session still writing through the old handle could
    // repopulate exactly what the purge just cleared. Giving up waiting is
    // not the same as being finished, so refusing here leaves the old account
    // intact rather than racing it: intact is a state the user can act on
    // again, and a purge run underneath a live writer is not.
    //
    // Every branch below decides the real `AccountExit`, never the bare
    // `AccountDisposition` a caller merely asked for: a supervisor that
    // respawned or dropped an id because a reset/remove was *requested*,
    // without checking whether it actually ran, would respawn an account
    // still holding its old state or forget one still sitting in `device`.
    // Read before the teardown below can change it: whether the *session's own
    // end* was the terminal credential state. A logout is not a failure worth
    // restarting — the server has rejected the stored credentials, and only
    // pairing again cures it — where every other natural end is a fault
    // isolation should ride out. Captured here because the teardown may clear
    // the account and take this with it.
    let logged_out = bridge.hub.is_logged_out();
    let exit = if let Some(disposition) = disposition {
        if !closed {
            log::error!(
                "local state was NOT reset: the session is still closing, and resetting the \
                 account from under it could let it repopulate what the reset just cleared. \
                 Start oxidezap again and repeat \"clear data and pair again\"."
            );
            disposition.incomplete()
        } else if !approvals_retired(account_id) {
            // The same refusal as above and for the same reason. What must not
            // outlive this account is the record of what its owner allowed: reset
            // the credentials first and fail this afterwards, and the next
            // pairing inherits an `approvals.json` in which a plugin with the
            // same id and mask is already allowed to act — consent given for an
            // account that no longer exists. Leaving the old account intact is a
            // state the user can act on again; a new account under the old one's
            // permissions is not. Its *settings* are cleared below with the rest
            // of the directory: those are data, and this is authority.
            log::error!(
                "local state was NOT reset: the plugins' recorded permissions could not be \
                 cleared, and resetting now would let them outlive the account that granted them. \
                 Start oxidezap again and repeat \"clear data and pair again\"."
            );
            disposition.incomplete()
        } else {
            // Purges this account's rows — upstream's and, through the
            // `ON DELETE CASCADE` the chat-store migration adds, this crate's
            // own. Never the whole-file wipe a single-account daemon used:
            // the shared database holds every other local account too, and
            // deleting it would take them down with this one.
            let completed = match disposition {
                AccountDisposition::Reset => {
                    match stores_for_reset.reset_account(account_id).await {
                        Ok(()) => {
                            log::info!("account {} reset; pair again", account_id.get());
                            true
                        }
                        Err(e) => {
                            log::error!("could not reset account {}: {e}", account_id.get());
                            false
                        }
                    }
                }
                AccountDisposition::Remove => {
                    match stores_for_reset.remove_account(account_id).await {
                        Ok(()) => {
                            log::info!("account {} removed", account_id.get());
                            true
                        }
                        Err(e) => {
                            log::error!("could not remove account {}: {e}", account_id.get());
                            false
                        }
                    }
                }
            };
            if !completed {
                disposition.incomplete()
            } else {
                // A plugin's own settings are this account's data too — an
                // autoreply's "already answered these people" is a list of
                // people — and they sit in their own directory beside the
                // plugins rather than inside the store. Nothing is writing
                // them any more: the threads were joined above.
                #[cfg(not(target_family = "wasm"))]
                if let Some(dir) = crate::plugins::account_state_dir(account_id)
                    && let Err(e) = std::fs::remove_dir_all(&dir)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    // Only the settings are at stake here: the permissions
                    // were retired before the credentials went, so nothing
                    // that survives this can let a plugin act on whoever
                    // pairs next.
                    log::error!("could not clear the plugins' stored settings: {e}");
                }
                // The store is one file; the media is a directory beside
                // it, and it is just as much this account's data.
                // Everything, staged uploads included: the account is
                // going, and so is anything that was going to be sent
                // under it.
                if let Err(e) = crate::media::AccountMedia::new(bridge.hub.account_id())
                    .wipe(crate::media::Wipe::Everything)
                {
                    log::error!("could not clear the media cache: {e}");
                }
                disposition.completed()
            }
        }
    } else if stopping || commands_closed {
        // The daemon is stopping, or nothing on this side can ever ask the
        // session for anything again. Not a fault to recover from: the
        // process is leaving, and `join_all` is waiting for exactly this.
        AccountExit::Stopped
    } else if logged_out {
        AccountExit::SessionLoggedOut
    } else {
        AccountExit::SessionEnded
    };
    crate::avatar::purge(&bridge.hub);
    Ok(exit)
}

/// How long to wait for the session to finish closing.
///
/// The thread has to disconnect the socket and close SQLite. Bounded so a
/// wedged session delays exit rather than preventing it: a daemon that will
/// not die has to be killed, which is worse than one that gave up waiting.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How long to wait when the store is about to be deleted.
///
/// Longer than the ordinary grace, because a wipe is only safe once the
/// session has actually let go of SQLite. Still bounded — a daemon that will
/// not die has to be killed — but here the answer to running out of patience
/// is to skip the wipe rather than to race it.
const FORGET_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// Everything the event loop carries between one event and the next.
struct Bridge {
    hub: Arc<StateHub>,
    lifecycle: RuntimeLifecycle,
    /// The plugins, fed the same events the front ends get.
    ///
    /// Held rather than reached for, because the bridge is also what tears
    /// them down: a plugin writing its settings while the account's data is
    /// being deleted is the same race the publish thread has, and it is
    /// solved the same way.
    plugins: Arc<oxidezap_plugin_host::Plugins>,
    /// Events on their way to the front ends that asked for them.
    ///
    /// A thread of its own, because preparing one writes every photo it
    /// carries to the cache: a history load is one event and hundreds of
    /// synchronous writes, and doing that on a runtime worker stops the accept
    /// loop, every connection task and the shutdown branch for its duration.
    /// One thread, and a queue, so the order the daemon publishes in is still
    /// the order things happened.
    ///
    /// `None` once the publisher has been asked to stop, which is the state
    /// that closes the channel: the thread ends when its last sender is gone.
    publish: Option<tokio::sync::mpsc::UnboundedSender<UiEvent>>,
    /// The publisher, kept joinable rather than detached. It writes the media
    /// a session event carries, and forgetting the session deletes exactly
    /// the directory it writes into.
    publisher: Option<crate::publisher::Handle>,
    reads: Arc<Mutex<ReadTracker>>,
    in_flight: Arc<Semaphore>,
}

impl Bridge {
    #[cfg(test)]
    fn new(hub: Arc<StateHub>, plugins: Arc<oxidezap_plugin_host::Plugins>) -> Self {
        Self::with_lifecycle(hub, plugins, RuntimeLifecycle::new())
    }

    fn with_lifecycle(
        hub: Arc<StateHub>,
        plugins: Arc<oxidezap_plugin_host::Plugins>,
        lifecycle: RuntimeLifecycle,
    ) -> Self {
        // Unbounded, and the bound that matters is upstream: the only producer
        // is the event loop draining the session's own unbounded channel, so a
        // limit here could only stall the loop this exists to unblock or drop
        // events no client could then recover.
        let (publish, queue) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();
        let publisher = crate::publisher::start(Arc::clone(&hub), queue);

        Self {
            hub,
            lifecycle,
            plugins,
            publish: Some(publish),
            publisher: Some(publisher),
            reads: Arc::new(Mutex::new(ReadTracker::default())),
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
        }
    }

    /// Close the publish queue and hand back the thread to wait on.
    ///
    /// Not a tidy-up. The publisher externalizes media — it writes this
    /// account's photos into the cache directory — and it runs behind an
    /// unbounded queue, so an event accepted before `ForgetSession` can still
    /// be in there. Deleting the directory while that thread is working
    /// through the backlog recreates the very bytes the wipe exists to
    /// remove, moments after it finishes.
    fn stop_publishing(&mut self) -> Option<crate::publisher::Handle> {
        // The thread ends when its last sender is gone, and this is it.
        self.publish = None;
        self.publisher.take()
    }

    fn reads(&self) -> std::sync::MutexGuard<'_, ReadTracker> {
        self.reads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
