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

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use oxidezap_core::UiEvent;
use oxidezap_session::WhatsAppClient;
use tokio::sync::Semaphore;

use crate::state::StateHub;

mod act;
mod action;
mod externalize;
mod read_tracker;
mod translate;

#[cfg(test)]
mod tests;

pub use action::{Action, CommandOutcome, Commands, Outbox, SessionCommand};
pub(crate) use externalize::externalize_media;

use act::MAX_IN_FLIGHT;
use read_tracker::ReadTracker;
use translate::Answer;

/// Whether the session is on its way out and must not be handed to anybody
/// new.
///
/// `ForgetSession` is deferred rather than done where it is accepted — the
/// file to delete is the one the session still has open — so between the
/// command being taken and the loop ending, this bridge is alive, reading
/// commands, and about to wipe the store. A caller that measured "alive" by
/// the command channel being open would attach to it and be served the
/// account it just asked to have deleted.
///
/// Process-global because a process has one session; the one reader is
/// [`crate::embedded`], which cannot see this bridge's own state.
static STOPPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the running session has begun going away.
pub fn stopping() -> bool {
    STOPPING.load(std::sync::atomic::Ordering::SeqCst)
}

/// Run the session until it ends or `shutdown` resolves.
///
/// Shutdown is a parameter rather than something the caller races this future
/// against: losing a `select!` would drop this future mid-await, and the
/// session would be torn down by `Drop` with nobody waiting for its thread to
/// disconnect and close SQLite. Owning the signal is what makes the teardown
/// below reachable on every exit path.
pub async fn run(
    hub: Arc<StateHub>,
    plugins: Arc<oxidezap_plugin_host::Plugins>,
    mut commands: tokio::sync::mpsc::Receiver<SessionCommand>,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    // This session is new, whatever the last one was doing.
    STOPPING.store(false, std::sync::atomic::Ordering::SeqCst);
    let mut client = WhatsAppClient::new().context("opening the local store")?;
    let mut events = client
        .start()
        .map_err(|e| anyhow::anyhow!("starting the session: {e}"))?;
    // Asked for once, here, rather than per front end: the session has one
    // camera and one call, and what decides whether a frame is *serialized*
    // is whether anybody is subscribed to the hub's video channel.
    let mut video = client.video_events();
    let mut bridge = Bridge::new(hub, plugins);

    // Set when every sender is gone. A closed channel yields `None`
    // immediately and forever, so leaving the branch enabled would spin the
    // loop at full speed instead of waiting for events.
    let mut commands_closed = false;

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
                    // Asked to forget: stop here so the teardown below runs
                    // before anything deletes the file it is closing.
                    if bridge.forget {
                        break;
                    }
                }
                None => commands_closed = true,
            },
            () = &mut shutdown => break,
        }
    }

    // Reached whether the session ended on its own or a signal arrived.
    //
    // Both of the things that would panic here — a join that blocks and the
    // drop of a tokio runtime inside an async context — belong to the client
    // rather than to this loop, so it does them: see `WhatsAppClient::close`.
    let grace = if bridge.forget {
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
    fn approvals_retired() -> bool {
        // A page keeps them in its origin's storage rather than in a
        // directory, and clears the plugins' settings in the same sweep:
        // there is no directory below to remove afterwards, so the two halves
        // that are separate on a desktop are one call here. What survives is
        // what survives there — the modules themselves.
        #[cfg(target_family = "wasm")]
        {
            oxidezap_plugin_host::Origin::forget_all()
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let Some(dir) = oxidezap_plugin_host::default_state_dir() else {
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

    // After the teardown, never before: the store is one file and the session
    // was holding it open. Unlinking it first leaves the closing session free
    // to write a fresh WAL beside a database that is already gone.
    // And only once it *has* torn down. Giving up waiting is not the same as
    // being finished: a session still closing can write a fresh WAL beside a
    // database that has just been unlinked, and the store is one file — a
    // partial wipe orphans everything behind the new device. Refusing to
    // delete leaves the old account intact, which is a state the user can act
    // on again; racing leaves one nobody can.
    if bridge.forget && !closed {
        log::error!(
            "local state was NOT wiped: the session is still closing, and deleting the store \
             from under it would leave a partial wipe. Start oxidezap again and repeat \
             \"clear data and pair again\"."
        );
    } else if bridge.forget && !approvals_retired() {
        // The same refusal as above and for the same reason. What must not
        // outlive this account is the record of what its owner allowed: wipe
        // the credentials first and fail this afterwards, and the next
        // pairing inherits an `approvals.json` in which a plugin with the
        // same id and mask is already allowed to act — consent given for an
        // account that no longer exists. Leaving the old account intact is a
        // state the user can act on again; a new account under the old one's
        // permissions is not. Its *settings* are cleared below with the rest
        // of the directory: those are data, and this is authority.
        log::error!(
            "local state was NOT wiped: the plugins' recorded permissions could not be \
             cleared, and wiping now would let them outlive the account that granted them. \
             Start oxidezap again and repeat \"clear data and pair again\"."
        );
    } else if bridge.forget {
        match oxidezap_session::wipe_local_state().await {
            Ok(()) => log::info!("local state wiped; pair again on the next start"),
            Err(e) => log::error!("could not wipe local state: {e}"),
        }
        // A plugin's own settings are this account's data too — an
        // autoreply's "already answered these people" is a list of people —
        // and they sit in their own directory beside the plugins rather than
        // inside the store. Nothing is writing them any more: the threads
        // were joined above.
        #[cfg(not(target_family = "wasm"))]
        if let Some(dir) = oxidezap_plugin_host::default_state_dir()
            && let Err(e) = std::fs::remove_dir_all(&dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            // Only the settings are at stake here: the permissions were
            // retired before the credentials went, so nothing that survives
            // this can let a plugin act on whoever pairs next.
            log::error!("could not clear the plugins' stored settings: {e}");
        }
        // The store is one file; the media is a directory beside it, and it
        // is just as much this account's data.
        // Everything, staged uploads included: the account is going, and so
        // is anything that was going to be sent under it.
        if let Err(e) = crate::media::wipe(crate::media::Wipe::Everything) {
            log::error!("could not clear the media cache: {e}");
        }
    }
    Ok(())
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
    /// Set by [`Action::ForgetSession`]. Read by the event loop, which stops
    /// and wipes once the session has let go of the store.
    forget: bool,
}

impl Bridge {
    fn new(hub: Arc<StateHub>, plugins: Arc<oxidezap_plugin_host::Plugins>) -> Self {
        // Unbounded, and the bound that matters is upstream: the only producer
        // is the event loop draining the session's own unbounded channel, so a
        // limit here could only stall the loop this exists to unblock or drop
        // events no client could then recover.
        let (publish, queue) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();
        let publisher = crate::publisher::start(Arc::clone(&hub), queue);

        Self {
            hub,
            plugins,
            publish: Some(publish),
            publisher: Some(publisher),
            reads: Arc::new(Mutex::new(ReadTracker::default())),
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            forget: false,
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
