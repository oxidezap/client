//! What one request becomes: an action for the session, and one answer back.
//!
//! Every request gets exactly one answer, including the ones that fail — a
//! client waiting on a reply that was never going to arrive is worse than a
//! client told no. Two shapes of answer live here and the difference is worth
//! keeping straight: most requests are acknowledged on the spot, while a
//! download, a page of history and a storage query are answered *later*, by
//! the task that does the work, under the id the client chose. See
//! [`addressed`] and [`out_of_band`], which are where that second shape is
//! written down once.

use std::sync::Arc;

use oxidezap_ipc::{CallAction, ClientRequest, DaemonMessage, ProtocolError, Request, RequestId};

use super::{always, error_frame};
use crate::session_bridge::{Action, CommandOutcome, Commands, Outbox, SessionCommand};
use crate::state::StateHub;

/// What the connection does with one request.
pub(super) struct Answer {
    /// The frame to send back, if there is one. Every request has one today;
    /// the option is what keeps a future fire-and-forget request from having
    /// to invent an acknowledgement.
    pub(super) frame: Option<String>,
    /// Whether to stop the daemon once that frame is on the wire.
    pub(super) shutdown: bool,
}

impl Answer {
    fn frame(frame: Option<String>) -> Self {
        Self {
            frame,
            shutdown: false,
        }
    }
}

/// The id an answer that arrives later has to be addressed to.
///
/// Most requests are acknowledged where they are read, so an id is a
/// convenience and doing without one costs the client nothing. For the few
/// whose answer is the thing asked for — a download's bytes, a page of rows,
/// the storage numbers — it is the address itself: the answer arrives among
/// every other answer this connection is owed, and one with no id on it is a
/// frame no waiter can claim. Refusing is the only honest answer, and
/// `needs` is what says which request was refused.
fn addressed(id: Option<RequestId>, needs: &'static str) -> Result<RequestId, Answer> {
    id.ok_or_else(|| {
        Answer::frame(always(
            None,
            error_frame(
                None,
                ProtocolError::Malformed {
                    detail: needs.into(),
                },
            ),
        ))
    })
}

/// Hand over an action whose real answer arrives later, under `id`.
///
/// Nothing is acknowledged: the bytes or the rows *are* the answer, and they
/// come back from the task the action spawns — so a second frame under the
/// same id now would be taken for it by a
/// client that has already retired its waiter. Only a refusal is answered
/// here, because then nothing else will be.
async fn out_of_band(hub: &StateHub, commands: &Commands, id: RequestId, action: Action) -> Answer {
    match dispatch(hub, commands, action).await {
        Ok(()) => Answer::frame(None),
        Err(error) => Answer::frame(always(Some(id), error_frame(Some(id), error))),
    }
}

/// Handle one request.
///
/// Every request gets exactly one answer, including the ones that fail: a
/// client waiting on a reply that was never going to arrive is worse than a
/// client told no.
pub(super) async fn handle_request(
    Request { id, request }: Request,
    hub: &StateHub,
    plugins: &Arc<oxidezap_plugin_host::Plugins>,
    commands: &Commands,
    outbox: &Outbox,
) -> Answer {
    // Every arm below answers under `id`, which is what lets a client match
    // an answer to the thing it asked — and why nothing here has to invent a
    // way to report a failure against the message a client happened to draw.
    let acted = |result| Answer::frame(answer(id, result));

    match request {
        ClientRequest::Snapshot => {
            Answer::frame(always(id, hub.hello_frame().map_err(anyhow::Error::from)))
        }
        // A second hello is harmless but says nothing; acknowledging keeps the
        // rule that every request gets exactly one answer.
        ClientRequest::Hello { .. } => acted(Ok(())),
        // The payload moves rather than being unpacked and rebuilt: the
        // request and the action carry the same struct, so there is nothing
        // here for a field to be dropped from.
        ClientRequest::SendText(request) => {
            acted(dispatch(hub, commands, Action::SendText(request)).await)
        }
        ClientRequest::SendAudio(request) => {
            acted(dispatch(hub, commands, Action::SendAudio(request)).await)
        }
        ClientRequest::SendMedia(request) => {
            acted(dispatch(hub, commands, Action::SendMedia(request)).await)
        }
        ClientRequest::Typing(request) => {
            acted(dispatch(hub, commands, Action::Typing(request)).await)
        }
        ClientRequest::Call(action) => acted(dispatch(hub, commands, Action::Call(action)).await),
        // The bytes come back as `Downloaded` under this id, seconds later,
        // from the task the action spawns.
        ClientRequest::Download(request) => {
            let id = match addressed(id, "a download needs an id to answer under") {
                Ok(id) => id,
                Err(refusal) => return refusal,
            };
            out_of_band(
                hub,
                commands,
                id,
                Action::Download {
                    id,
                    request,
                    answer_to: outbox.clone(),
                },
            )
            .await
        }
        ClientRequest::ReloadHistory => acted(dispatch(hub, commands, Action::ReloadHistory).await),
        // Answered with the page under this id, like a download and for the
        // same reason.
        ClientRequest::LoadMessages(request) => {
            let id = match addressed(id, "a page needs an id to answer under") {
                Ok(id) => id,
                Err(refusal) => return refusal,
            };
            out_of_band(
                hub,
                commands,
                id,
                Action::LoadMessages {
                    id,
                    request,
                    answer_to: outbox.clone(),
                },
            )
            .await
        }
        ClientRequest::LoadChats(request) => {
            let id = match addressed(id, "a page needs an id to answer under") {
                Ok(id) => id,
                Err(refusal) => return refusal,
            };
            out_of_band(
                hub,
                commands,
                id,
                Action::LoadChats {
                    id,
                    request,
                    answer_to: outbox.clone(),
                },
            )
            .await
        }
        ClientRequest::ForgetSession => acted(dispatch(hub, commands, Action::ForgetSession).await),
        ClientRequest::MarkRead(request) => {
            acted(dispatch(hub, commands, Action::MarkRead(request)).await)
        }
        ClientRequest::MarkStatusWatched(request) => {
            acted(dispatch(hub, commands, Action::MarkStatusWatched(request)).await)
        }
        // Measured here rather than by the client: the daemon is the only
        // process that opens the store or writes the media cache, so a front
        // end asking the filesystem would be guessing at paths it does not
        // own. No session needed — this is two directory reads.
        ClientRequest::StorageUsage => {
            // Answered under an id like a download, because the numbers are
            // the answer rather than an acknowledgement of it — though this
            // one is measured here rather than by a task, so the frame goes
            // back on the connection's own writer.
            let id = match addressed(id, "a storage query needs an id to answer under") {
                Ok(id) => id,
                Err(refusal) => return refusal,
            };
            // Two directory walks, off the runtime for the same reason the
            // clear is.
            let measured = oxidezap_session::unblock(|| {
                let (media_bytes, media_files) = crate::media::cache_usage();
                (database_bytes(), media_bytes, media_files)
            })
            .await;
            let (database_bytes, media_bytes, media_files) = measured.unwrap_or((0, 0, 0));
            Answer::frame(always(
                Some(id),
                serde_json::to_string(&DaemonMessage::Storage {
                    id,
                    database_bytes,
                    media_bytes,
                    media_files,
                })
                .map_err(anyhow::Error::from),
            ))
        }
        // The store stays; every message keeps its `downloadable`, so what
        // this costs is a re-download of whatever is looked at again.
        ClientRequest::ClearMediaCache => {
            // Off the runtime, for the reason the plugin approval is: this
            // reads a directory of up to half a gigabyte and deletes it file
            // by file, holding a lock that the session's own publish thread
            // takes for every photo it caches. Done here it stopped event
            // delivery for as long as a slow disk took. Awaited rather than
            // spawned loose, so the acknowledgement still means the cache is
            // clear.
            let cleared = oxidezap_session::unblock(|| {
                // Cached downloads only: a staged upload belongs to a send
                // that has not run yet. See `media::Wipe`.
                crate::media::wipe(crate::media::Wipe::Cache).map_err(|e| e.to_string())
            })
            .await;
            acted(match cleared {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(ProtocolError::Malformed {
                    detail: format!("could not clear the media cache: {e}"),
                }),
                Err(_) => Err(ProtocolError::Malformed {
                    detail: "the media cache was not cleared".to_string(),
                }),
            })
        }
        // Applied here and now, and written down for the next start. Both,
        // because they answer different questions: a person raising the
        // level is asking about the session that is running, and the file is
        // what keeps them from having to ask again after every restart.
        //
        // The write is off the runtime for the reason the plugin approval's
        // is — it is a file created, flushed and renamed, which on a
        // single-worker runtime stalls the session bridge and every other
        // connection for as long as the disk takes. Awaited rather than
        // spawned loose, so the acknowledgement means the choice is on disk.
        ClientRequest::SetLogLevel { level } => {
            oxidezap_logging::apply(level);
            log::info!("logging at {level}, asked for by a front end");
            // `remember` writes the level in force rather than this
            // request's, and serializes the writes: two front ends can ask in
            // the same moment and each is written on a thread of its own, so
            // a write carrying its own level could land after a later one and
            // leave the next start at the earlier answer.
            let recorded = oxidezap_session::unblock(oxidezap_logging::remember).await;
            match recorded {
                Ok(Ok(())) => acted(Ok(())),
                // The level *did* change; only the memory of it failed. Said
                // in the log rather than refused, because answering `Refused`
                // to a request that was carried out is the worse lie of the
                // two.
                //
                // At `error` rather than `warn`, which the plugin approval's
                // failed record is written at for the same reason: this line
                // is written *after* the level it reports on has taken
                // effect, so somebody quieting the daemon to `error` would
                // otherwise have the one thing worth telling them dropped by
                // the level they just chose. At `off` it is dropped, and that
                // is what `off` means.
                Ok(Err(e)) => {
                    log::error!("the log level was changed but not stored: {e}");
                    acted(Ok(()))
                }
                Err(_) => {
                    log::error!("the log level was changed but the store was not reached");
                    acted(Ok(()))
                }
            }
        }
        // The daemon has no window of its own, so this is relayed rather than
        // acted on: whoever owns a window is the only one that can raise it.
        // Published to every client, including the one that asked, because a
        // front end that sent this on a user's behalf wants the window up
        // regardless of which process is holding it. Through the same door as
        // the tray's Open, so that "there should be a window" means the same
        // thing however it was asked — including when there is none to raise.
        ClientRequest::ShowWindow => {
            crate::window::show(hub);
            acted(Ok(()))
        }
        // Not dispatched to the session: a plugin action touches the account
        // only if the plugin decides it should, and what it decides is its
        // own business. Handing it over is the whole of the daemon's part,
        // which is why this answers `Accepted` rather than waiting — the
        // plugin's own answer reaches it inside the sandbox, where a socket
        // front end's never could.
        ClientRequest::PluginAction { action } => {
            plugins.act(&action);
            acted(Ok(()))
        }
        // The one thing about a plugin that a plugin has no say in. Answered
        // rather than dispatched, like the action above: what the plugin does
        // with its new permissions is its own business and arrives as a
        // republished surface.
        ClientRequest::PluginApproval { plugin, approved } => {
            // Where it is recorded is `plugins::approve`, which is a platform
            // split: a desktop writes and renames a file and so must leave the
            // runtime's thread, and a page writes `localStorage` and has no
            // blocking pool to leave for — `spawn_blocking` here panicked
            // outright in a browser, so approving a plugin there never worked.
            // Awaited either way, so the acknowledgement still means the
            // answer is recorded.
            let recorded = crate::plugins::approve(plugins, plugin, approved).await;
            if recorded {
                acted(Ok(()))
            } else {
                acted(Err(ProtocolError::Refused {
                    detail: "the approval could not be recorded".to_string(),
                }))
            }
        }
        // Taken, and answered now rather than when it finishes. Awaiting it
        // here was wrong for one reason and it is not a small one: this is the
        // connection's own loop, so a reload of a folder that takes seconds
        // is seconds in which *that window* is served nothing — no state, no
        // session events, no call video, which is eight frames deep and
        // overflows almost at once. And nothing was waiting for the answer
        // anyway: the set that comes back is state, and every window learns
        // of it in the same frame, because a plugin's interface was always
        // the daemon's rather than the asking window's.
        ClientRequest::ReloadPlugins => {
            crate::plugins::reload_in_background(plugins);
            acted(Ok(()))
        }
        // The acknowledgement goes out first; see the caller.
        ClientRequest::Shutdown => Answer {
            frame: answer(id, Ok(())),
            shutdown: true,
        },
    }
}

/// Hand a command to the session and wait for what became of it.
///
/// Waiting, rather than answering on admission to the queue, is what makes
/// `Accepted` mean something: the account can drop between the check here and
/// the moment the bridge picks the command up, and a client told yes on
/// admission would never learn its message went nowhere. It is also the
/// backpressure — a connection has one command outstanding at a time, so the
/// client cap is also the cap on queued work.
pub(super) async fn dispatch(
    hub: &StateHub,
    commands: &Commands,
    action: Action,
) -> Result<(), ProtocolError> {
    // Refused early as well as late: a client that is watching the connection
    // state should get the answer it can already predict, without the round
    // trip. Only for what actually needs the network — see
    // [`Action::needs_network`].
    let connection = hub.connection();
    if action.needs_network() && !connection.is_connected() {
        // A call the asking window already drew has to be un-drawn. It
        // passed its own connection check before this one moved, the refusal
        // rides no request id, and nothing on that side connects the error
        // back to the stage it is holding — so the stage would sit there
        // until the next snapshot dropped it, and disappearing is what a
        // front end writes down as an attempt that was never answered. The
        // bridge's busy refusal says the same thing one layer down.
        if let Action::Call(CallAction::Start { placeholder_id, .. }) = &action {
            hub.calls(|calls| calls.mark_unrecorded(placeholder_id));
            hub.republish_calls();
        }
        return Err(no_session(format!("not connected: {connection:?}")));
    }

    let (reply, answer) = tokio::sync::oneshot::channel();
    if commands
        .send(SessionCommand { action, reply })
        .await
        .is_err()
    {
        // The bridge is gone: the daemon is on its way down.
        return Err(no_session("the session is shutting down"));
    }

    match answer.await {
        Ok(CommandOutcome::Accepted) => Ok(()),
        Ok(CommandOutcome::NoSession(detail)) => Err(no_session(detail)),
        Ok(CommandOutcome::Refused(detail)) => Err(ProtocolError::Refused { detail }),
        // The bridge took the command and died before answering.
        Err(_) => Err(no_session("the session stopped before it answered")),
    }
}

/// The frame that answers a command, whichever way it went.
///
/// One place, because with an id on every answer there is nothing left to
/// special-case: a refusal is an error naming its request, exactly like a
/// refused download or a malformed frame.
fn answer(id: Option<RequestId>, result: Result<(), ProtocolError>) -> Option<String> {
    match result {
        Ok(()) => always(
            id,
            serde_json::to_string(&DaemonMessage::Accepted { id }).map_err(anyhow::Error::from),
        ),
        Err(error) => always(id, error_frame(id, error)),
    }
}

/// The store's footprint: the database plus the journal files SQLite would
/// replay into it. All three are the same data, so all three are counted.
///
/// # Zero on a page, deliberately
///
/// A browser's database is in a VFS rather than on a filesystem, so every
/// `metadata` here fails and the sum is 0 — which Settings shows as `0 B`.
/// Wrong, and it is the least bad of the three answers available. The size is
/// `page_count * page_size`, which needs a query, and this handler is
/// synchronous by the shape of the protocol; the VFS's own `export_db` would
/// answer by copying the whole database into memory, which is precisely what
/// everything else on this side goes out of its way not to do. Fixing it
/// properly means an async usage query through `session/store/`, and that is
/// a wider change than a number in a settings pane is worth today. Recorded
/// in `docs/roadmap.md`.
fn database_bytes() -> u64 {
    let base = oxidezap_session::resolve_database_path();
    ["", "-wal", "-shm"]
        .iter()
        .filter_map(|suffix| std::fs::metadata(format!("{base}{suffix}")).ok())
        .map(|meta| meta.len())
        .sum()
}

fn no_session(detail: impl Into<String>) -> ProtocolError {
    ProtocolError::NoSession {
        detail: detail.into(),
    }
}
