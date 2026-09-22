//! Carrying one client command out.
//!
//! Everything between taking an [`Action`] off the channel and the answer the
//! asking connection gets: what may run at all, what runs here, what is handed
//! to a task of its own, and how many of those may be in flight at once.

use std::sync::Arc;

use oxidezap_core::CallOutcome;
use oxidezap_ipc::{
    CallAction, ChatSummary, DaemonEvent, DaemonMessage, PageCursor, ProtocolError, RequestId,
};
use oxidezap_session::{ReadBoundary, WhatsAppClient};
use tokio::sync::OwnedSemaphorePermit;
use wacore_binary::jid::observe_str;

use super::externalize::externalize_messages;
use super::read_tracker::ReadRecord;
use super::translate::chat_updated;
use super::wire_events::{chat_message_to_dto, chat_to_dto};
use super::{Action, Bridge, CommandOutcome, Outbox, SessionCommand};
use crate::state::Change;
use oxidezap_wire::dto::{
    ChannelDto, ChatDto, ContactDto, GroupDto, GroupJoinRequestDto,
    GroupParticipantAction as WireParticipantAction, GroupParticipantDto, MessageDto, PollDto,
    PollOptionDto, PresenceState,
};
use oxidezap_wire::envelope::ResponseEnvelope;
use oxidezap_wire::error::ApiError;
use oxidezap_wire::request::ClientRequest as WireRequest;
use oxidezap_wire::response::DaemonResponse;

/// How many commands may still be working inside the session at once.
///
/// The command channel bounds admission to this loop, not the network work it
/// starts: every session call spawns and returns, so without this a client
/// that reads its acknowledgements as fast as it sends could keep queueing
/// sends until the machine gave out. A permit is held until the work it paid
/// for finishes, and a command that cannot get one is refused rather than
/// queued — a front end told to retry is in a better position than one whose
/// request is sitting in a queue it cannot see.
pub(super) const MAX_IN_FLIGHT: usize = 64;

impl Bridge {
    /// Act on one client command, and answer the connection that asked.
    ///
    /// Async because one action is finished when its answer is: recording a
    /// status view writes a row and nothing else, and a client told
    /// `Accepted` before that row exists could see it lost to the very
    /// teardown the answer outran. Everything else still hands work to the
    /// session and returns.
    pub(super) async fn execute(&mut self, client: &WhatsAppClient, command: SessionCommand) {
        let SessionCommand { action, reply } = command;
        // The store reads answer from tasks of their own; what comes back here
        // is everything else.
        let Some((action, reply)) = self.begin_slow(client, action, reply) else {
            return;
        };
        let outcome = self.act(client, action).await;
        // A refusal nobody is listening for is not worth logging: the client
        // hung up, which is its right.
        let _ = reply.send(outcome);
    }

    /// Start the actions whose work is a store round trip, and answer from a
    /// task of their own. Gives the command back when it is not one of them.
    ///
    /// Awaited in the run loop, one of these stops the loop for as long as
    /// SQLite takes: a signal is not observed while it runs, and the video
    /// channel — four frames deep, with this loop its only consumer —
    /// overflows into dropped frames and keyframe requests for the length of
    /// a local query. What the fold needs is shared rather than borrowed, so
    /// what the daemon learns from a page it serves is learned wherever the
    /// page lands.
    ///
    /// A page read takes a permit like every other slow action: the command
    /// channel has no bound of its own, and a front end that reads its
    /// acknowledgements would otherwise keep queueing SQLite reads, each
    /// holding its page and that page's externalized media until it answers.
    fn begin_slow(
        &mut self,
        client: &WhatsAppClient,
        action: Action,
        reply: tokio::sync::oneshot::Sender<CommandOutcome>,
    ) -> Option<(Action, tokio::sync::oneshot::Sender<CommandOutcome>)> {
        match action {
            Action::MarkStatusWatched(oxidezap_ipc::MarkStatusWatched { message_ids }) => {
                // The other actions are finished when the session has taken
                // them and what the network makes of them arrives later; this
                // one *is* the write, there is no retry, and the answer is the
                // only thing that can tell a window its ring is coming back.
                let written = client.mark_status_watched(message_ids);
                oxidezap_session::spawn(async move {
                    let outcome = match written.await {
                        Ok(true) => CommandOutcome::Accepted,
                        Ok(false) => CommandOutcome::Refused(
                            "the status view could not be recorded".to_string(),
                        ),
                        Err(e) => CommandOutcome::Refused(format!(
                            "the status view could not be recorded: {e}"
                        )),
                    };
                    let _ = reply.send(outcome);
                });
                None
            }
            Action::LoadMessages {
                id,
                request: oxidezap_ipc::LoadMessages { jid, before, limit },
                answer_to,
            } => {
                // Before the call, not after it: `load_messages` spawns the
                // query as it returns, so taking the permit afterwards refused
                // the request and ran it anyway.
                let Some(permit) = self.permit() else {
                    let _ = reply.send(too_busy());
                    return None;
                };
                let page = client.load_messages(
                    jid.clone(),
                    before.map(|cursor| cursor.as_str().to_string()),
                    limit.map_or(WhatsAppClient::MESSAGE_PAGE, i64::from),
                );
                let reads = Arc::clone(&self.reads);
                // The cache epoch as it is *now*, not as it will be when the
                // page lands. A `ForgetSession` in between retires this one,
                // and `put_since` then refuses the write rather than putting
                // the departed account's thumbnails back into a directory the
                // wipe has already emptied. This account's own epoch.
                let account_media = crate::media::AccountMedia::new(self.hub.account_id());
                let epoch = account_media.epoch();
                // Which account asked. A page of the old one's history
                // landing after it left would be folded into a tracker that
                // had just forgotten it, and the next account would carry the
                // previous one's boundaries. See [`StateHub::forget_account`].
                let asked_as = self.hub.account_generation();
                let hub = Arc::clone(&self.hub);
                oxidezap_session::spawn(async move {
                    let answer = match page.await {
                        Ok(Ok(mut page)) => {
                            // The bytes travel the way they do everywhere
                            // else: written to the media directory, named by
                            // a key.
                            externalize_messages(&account_media, epoch, &mut page.items);
                            // What this side served, it now knows. A read is
                            // bounded by the messages the daemon has observed,
                            // and the page a front end asked for is the
                            // history it is about to read: without this, a
                            // window naming a message from a page nobody told
                            // the daemon about is refused, and the badge comes
                            // back on the next hydration.
                            //
                            // Asked with the tracker's own lock held, which
                            // is what makes the answer good enough: a logout
                            // clears the tracker *after* it bumps the
                            // generation, so either this reads the bump and
                            // folds nothing, or it folds and the clear that
                            // follows takes it.
                            {
                                let mut reads =
                                    reads.lock().unwrap_or_else(|held| held.into_inner());
                                if hub.account_generation() == asked_as {
                                    for message in &page.items {
                                        reads.observe_message(&jid, message);
                                    }
                                }
                            }
                            Ok(DaemonMessage::Messages {
                                id,
                                jid,
                                messages: page.items,
                                next: page.next.map(PageCursor::new),
                            })
                        }
                        // The store read failed, which is not the client's
                        // frame and not something it could ask differently:
                        // a page it asks for again is a query that could well
                        // answer. See `finish_download`.
                        Ok(Err(detail)) => Err(ProtocolError::Failed {
                            detail,
                            retryable: true,
                        }),
                        Err(_) => Err(ProtocolError::NoSession {
                            detail: "the session stopped before the page arrived".to_string(),
                        }),
                    };
                    answer_now(&answer_to, answered(id, answer));
                    let _ = reply.send(CommandOutcome::Accepted);
                    drop(permit);
                });
                None
            }
            Action::LoadChats {
                id,
                request:
                    oxidezap_ipc::LoadChats {
                        after,
                        limit,
                        archived,
                    },
                answer_to,
            } => {
                // As above: the permit is what decides whether the query
                // runs, so it is taken before the call that starts one.
                let Some(permit) = self.permit() else {
                    let _ = reply.send(too_busy());
                    return None;
                };
                let page = client.load_chats(
                    after.map(|cursor| cursor.as_str().to_string()),
                    limit.map_or(WhatsAppClient::CHAT_PAGE, i64::from),
                    archived,
                );
                let reads = Arc::clone(&self.reads);
                let hub = Arc::clone(&self.hub);
                // As above: taken now, so a wipe between the ask and the
                // answer refuses the write rather than repopulating the cache.
                let account_media = crate::media::AccountMedia::new(hub.account_id());
                let epoch = account_media.epoch();
                // As above: a page of the departed account's chats must not
                // be put back into a hub that has just been emptied of it.
                let asked_as = hub.account_generation();
                oxidezap_session::spawn(async move {
                    let answer = match page.await {
                        Ok(Ok(mut page)) => {
                            // The same rule. A chat past the attach window is
                            // in no snapshot, and a read for one is refused
                            // with "no such chat" until this side has been
                            // told it exists. Its rows are learned as well as
                            // its summary: a window opening such a chat names
                            // the message it can see, and a read naming a
                            // message this side has never observed is refused
                            // for having no boundary — a badge that clears
                            // locally, sends no receipt and comes straight
                            // back on the next hydration.
                            for chat in &mut page.items {
                                externalize_messages(&account_media, epoch, &mut chat.messages);
                                let mut reads =
                                    reads.lock().unwrap_or_else(|held| held.into_inner());
                                for message in &chat.messages {
                                    reads.observe_message(&chat.jid, message);
                                }
                                // The hub is the active-list snapshot handed
                                // to newly attached windows. An explicit
                                // include-archived page belongs only to the
                                // requesting window; publishing its archived
                                // rows here would resurrect them as ordinary
                                // placeholder chats on the next attach.
                                if chat.archived {
                                    hub.apply_for(
                                        asked_as,
                                        Change::from_store(DaemonEvent::ChatRemoved {
                                            jid: chat.jid.clone(),
                                        }),
                                    );
                                    // An archived row has no active read
                                    // boundary, even if this generation's
                                    // removal was accepted by the hub.
                                    reads.forget(&chat.jid);
                                    continue;
                                }
                                // Asked and written under one lock, so a
                                // logout cannot land between the question and
                                // the answer. What it refuses is folded back
                                // out of the tracker: an entry for a departed
                                // account's chat is a boundary the next one
                                // would inherit.
                                let summary = chat_updated(chat, &mut reads);
                                if !hub.apply_for(asked_as, summary) {
                                    reads.forget(&chat.jid);
                                }
                            }
                            Ok(DaemonMessage::Chats {
                                id,
                                chats: page.items,
                                next: page.next.map(PageCursor::new),
                            })
                        }
                        // The store read failed, which is not the client's
                        // frame and not something it could ask differently:
                        // a page it asks for again is a query that could well
                        // answer. See `finish_download`.
                        Ok(Err(detail)) => Err(ProtocolError::Failed {
                            detail,
                            retryable: true,
                        }),
                        Err(_) => Err(ProtocolError::NoSession {
                            detail: "the session stopped before the page arrived".to_string(),
                        }),
                    };
                    answer_now(&answer_to, answered(id, answer));
                    let _ = reply.send(CommandOutcome::Accepted);
                    drop(permit);
                });
                None
            }
            Action::GroupMembers {
                id,
                request: oxidezap_ipc::GroupMembers { jid },
                answer_to,
            } => {
                // A permit before the call, as above: `group_roster` spawns
                // its work as it returns, so one taken afterwards would
                // refuse the request and run it anyway.
                let Some(permit) = self.permit() else {
                    let _ = reply.send(too_busy());
                    return None;
                };
                let roster = client.group_roster(jid);
                oxidezap_session::spawn(async move {
                    let answer = match roster.await {
                        Ok(Ok(roster)) => Ok(DaemonMessage::GroupMembers { id, roster }),
                        // Not the client's frame: the address it named is a
                        // group it is in, and what failed is a query behind
                        // it. A group whose list has to be fetched is one
                        // whose fetch can well answer next time.
                        Ok(Err(detail)) => Err(ProtocolError::Failed {
                            detail,
                            retryable: true,
                        }),
                        Err(_) => Err(ProtocolError::NoSession {
                            detail: "the session stopped before the members arrived".to_string(),
                        }),
                    };
                    answer_now(&answer_to, answered(id, answer));
                    let _ = reply.send(CommandOutcome::Accepted);
                    drop(permit);
                });
                None
            }
            Action::Wire {
                id,
                request,
                answer_to,
            } => {
                let Some(permit) = self.permit() else {
                    let _ = reply.send(too_busy());
                    return None;
                };
                self.begin_slow_wire(client, id, request, answer_to, reply, permit);
                None
            }
            action => Some((action, reply)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn begin_slow_wire(
        &mut self,
        client: &WhatsAppClient,
        id: u64,
        request: oxidezap_wire::request::ClientRequest,
        answer_to: Outbox,
        reply: tokio::sync::oneshot::Sender<CommandOutcome>,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) {
        match request {
            WireRequest::ListMessages {
                chat_jid,
                limit,
                before,
                after,
            } => {
                // One cursor or the other, never both: `after` walks forward
                // from a row the caller holds, `before` walks back from the
                // newest. A request carrying both is answered as the forward
                // page, because that is the one whose position the caller
                // cannot infer.
                let page = match after {
                    Some(after) => {
                        client.load_messages_after(chat_jid.clone(), after, limit as i64)
                    }
                    None => client.load_messages(chat_jid.clone(), before, limit as i64),
                };
                let answer_to = answer_to.clone();
                oxidezap_session::spawn(async move {
                    let result = match page.await {
                        Ok(Ok(page)) => {
                            let messages: Vec<MessageDto> = page
                                .items
                                .into_iter()
                                .map(|m| chat_message_to_dto(&chat_jid, m))
                                .collect();
                            let next_cursor =
                                page.next.map(oxidezap_wire::envelope::PageCursor::new);
                            Ok(DaemonResponse::Messages {
                                messages,
                                next_cursor,
                            })
                        }
                        Ok(Err(e)) => Err(ApiError::internal(e)),
                        Err(e) => Err(ApiError::not_connected(e.to_string())),
                    };
                    send_wire_result(&answer_to, id, result);
                    let _ = reply.send(CommandOutcome::Accepted);
                    drop(permit);
                });
            }
            WireRequest::GetMessage {
                chat_jid,
                message_id,
            } => {
                let task = client.get_message(chat_jid.clone(), message_id);
                let answer_to = answer_to.clone();
                oxidezap_session::spawn(async move {
                    let result = match task.await {
                        Ok(Ok(Some(msg))) => {
                            Ok(DaemonResponse::Message(chat_message_to_dto(&chat_jid, msg)))
                        }
                        Ok(Ok(None)) => Err(ApiError::not_found("message not found")),
                        Ok(Err(e)) => Err(ApiError::internal(e)),
                        Err(e) => Err(ApiError::not_connected(e.to_string())),
                    };
                    send_wire_result(&answer_to, id, result);
                    let _ = reply.send(CommandOutcome::Accepted);
                    drop(permit);
                });
            }
            WireRequest::GetMessageContext {
                chat_jid,
                message_id,
                limit,
            } => {
                let task = client.get_message_context(chat_jid.clone(), message_id, limit);
                let answer_to = answer_to.clone();
                oxidezap_session::spawn(async move {
                    let result = match task.await {
                        Ok(Ok(msgs)) => {
                            let messages: Vec<MessageDto> = msgs
                                .into_iter()
                                .map(|m| chat_message_to_dto(&chat_jid, m))
                                .collect();
                            Ok(DaemonResponse::Messages {
                                messages,
                                next_cursor: None,
                            })
                        }
                        Ok(Err(e)) => Err(ApiError::internal(e)),
                        Err(e) => Err(ApiError::not_connected(e.to_string())),
                    };
                    send_wire_result(&answer_to, id, result);
                    let _ = reply.send(CommandOutcome::Accepted);
                    drop(permit);
                });
            }
            WireRequest::SearchMessages {
                query,
                chat_jid,
                has_media,
                limit,
            } => {
                let task = client.search_messages(query, chat_jid.clone(), has_media, limit as i64);
                let answer_to = answer_to.clone();
                oxidezap_session::spawn(async move {
                    let result = match task.await {
                        Ok(Ok(msgs)) => {
                            let messages: Vec<MessageDto> = msgs
                                .into_iter()
                                .map(|m| {
                                    let c_jid =
                                        chat_jid.as_deref().unwrap_or(&m.sender).to_string();
                                    chat_message_to_dto(&c_jid, m)
                                })
                                .collect();
                            Ok(DaemonResponse::Messages {
                                messages,
                                next_cursor: None,
                            })
                        }
                        Ok(Err(e)) => Err(ApiError::internal(e)),
                        Err(e) => Err(ApiError::not_connected(e.to_string())),
                    };
                    send_wire_result(&answer_to, id, result);
                    let _ = reply.send(CommandOutcome::Accepted);
                    drop(permit);
                });
            }
            WireRequest::ListContacts { query, limit } => {
                let task = client.list_contact_views(query, limit as i64);
                let answer_to = answer_to.clone();
                oxidezap_session::spawn(async move {
                    let result = match task.await {
                        Ok(Ok(contacts)) => {
                            let contacts: Vec<ContactDto> =
                                contacts.into_iter().map(contact_to_dto).collect();
                            Ok(DaemonResponse::Contacts { contacts })
                        }
                        Ok(Err(e)) => Err(ApiError::internal(e)),
                        Err(e) => Err(ApiError::not_connected(e.to_string())),
                    };
                    send_wire_result(&answer_to, id, result);
                    let _ = reply.send(CommandOutcome::Accepted);
                    drop(permit);
                });
            }
            WireRequest::ListChats {
                limit,
                offset,
                query,
                archived,
            } => {
                let task = client.load_chats(offset, limit as i64, archived);
                let answer_to = answer_to.clone();
                oxidezap_session::spawn(async move {
                    let result = match task.await {
                        Ok(Ok(page)) => {
                            let chats: Vec<ChatDto> = page
                                .items
                                .into_iter()
                                .filter(|chat| match &query {
                                    None => true,
                                    Some(q) => {
                                        chat.jid.contains(q.as_str())
                                            || chat.name.to_lowercase().contains(&q.to_lowercase())
                                    }
                                })
                                .map(chat_to_dto)
                                .collect();
                            let next_cursor =
                                page.next.map(oxidezap_wire::envelope::PageCursor::new);
                            Ok(DaemonResponse::Chats { chats, next_cursor })
                        }
                        Ok(Err(e)) => Err(ApiError::internal(e)),
                        Err(e) => Err(ApiError::not_connected(e.to_string())),
                    };
                    send_wire_result(&answer_to, id, result);
                    let _ = reply.send(CommandOutcome::Accepted);
                    drop(permit);
                });
            }
            WireRequest::GetChat { jid } => {
                let task = client.get_chat(jid);
                let answer_to = answer_to.clone();
                oxidezap_session::spawn(async move {
                    let result = match task.await {
                        Ok(Ok(Some(chat))) => Ok(DaemonResponse::Chat(chat_to_dto(chat))),
                        Ok(Ok(None)) => Err(ApiError::not_found("no such chat")),
                        Ok(Err(e)) => Err(ApiError::internal(e)),
                        Err(e) => Err(ApiError::not_connected(e.to_string())),
                    };
                    send_wire_result(&answer_to, id, result);
                    let _ = reply.send(CommandOutcome::Accepted);
                    drop(permit);
                });
            }
            WireRequest::SendText {
                to,
                message,
                reply_to,
                mentions,
                enqueue_only,
            } => {
                if enqueue_only {
                    // Dispatched, not awaited: the GUI's own acknowledgement
                    // semantics. The id is announced by the send itself, so
                    // the id the client is answered with is the same one the
                    // store records and WhatsApp is told. Waited for through a
                    // timeout: a session that dies before it picks the id must
                    // answer, not hang. The task is created here, so what the
                    // spawned future owns is the `Task` rather than the
                    // borrowed client.
                    let (announce, announced) = tokio::sync::oneshot::channel();
                    let pending =
                        client.send_text_wire(to, message, reply_to, mentions, Some(announce));
                    let answer_to = answer_to.clone();
                    oxidezap_session::spawn(async move {
                        let announced = oxidezap_session::with_timeout(
                            announced,
                            std::time::Duration::from_secs(10),
                        )
                        .await;
                        match announced {
                            Some(Ok(local_id)) => {
                                let answer = DaemonResponse::MessageSent {
                                    id: local_id,
                                    timestamp_ms: wacore::time::now_millis(),
                                    enqueued: true,
                                };
                                send_wire_result(&answer_to, id, Ok(answer));
                                let _ = reply.send(CommandOutcome::Accepted);
                                if let Ok(Err(e)) = pending.await {
                                    log::warn!("an enqueued send failed after its answer: {e}");
                                }
                            }
                            _ => {
                                // No id arrived. A send that refuses before
                                // it starts — a bad JID, empty text — reports
                                // a request error; a session that stopped, a
                                // task that died before announcing, or a wait
                                // that expired did not, and folding the two
                                // together told a client its request was
                                // wrong when the session was gone.
                                let settled = oxidezap_session::with_timeout(
                                    pending,
                                    std::time::Duration::from_secs(5),
                                )
                                .await;
                                let result = match settled {
                                    Some(Ok(Err(e))) => Err(ApiError::invalid_argument(e)),
                                    // The send finished in the gap between
                                    // the announcement's deadline and this
                                    // await: it succeeded, so answer with
                                    // what it produced rather than an error.
                                    Some(Ok(Ok((sent_id, ts)))) => {
                                        Ok(DaemonResponse::MessageSent {
                                            id: sent_id,
                                            timestamp_ms: ts,
                                            enqueued: true,
                                        })
                                    }
                                    _ => Err(ApiError::not_connected(
                                        "the session stopped before the send started",
                                    )),
                                };
                                send_wire_result(&answer_to, id, result);
                                let _ = reply.send(CommandOutcome::Accepted);
                            }
                        }
                        drop(permit);
                    });
                } else {
                    let task = client.send_text_wire(to, message, reply_to, mentions, None);
                    run_wire(
                        answer_to.clone(),
                        id,
                        reply,
                        permit,
                        task,
                        |(sent_id, ts)| DaemonResponse::MessageSent {
                            id: sent_id,
                            timestamp_ms: ts,
                            enqueued: false,
                        },
                    );
                }
            }
            WireRequest::SendReaction {
                chat_jid,
                message_id,
                emoji,
            } => {
                let task = client.send_reaction(chat_jid, message_id, emoji);
                run_wire(answer_to.clone(), id, reply, permit, task, |sent_id| {
                    DaemonResponse::MessageSent {
                        id: sent_id,
                        timestamp_ms: wacore::time::now_millis(),
                        enqueued: false,
                    }
                });
            }
            WireRequest::SendMedia {
                to,
                file_path,
                caption,
                mime_type,
                as_document,
            } => {
                let task =
                    client.send_media_from_path(to, file_path, caption, mime_type, as_document);
                run_wire(
                    answer_to.clone(),
                    id,
                    reply,
                    permit,
                    task,
                    |(sent_id, ts)| DaemonResponse::MessageSent {
                        id: sent_id,
                        timestamp_ms: ts,
                        enqueued: false,
                    },
                );
            }
            WireRequest::SendAudio { to, file_path, ptt } => {
                let task = client.send_audio_from_path(to, file_path, ptt);
                run_wire(
                    answer_to.clone(),
                    id,
                    reply,
                    permit,
                    task,
                    |(sent_id, ts)| DaemonResponse::MessageSent {
                        id: sent_id,
                        timestamp_ms: ts,
                        enqueued: false,
                    },
                );
            }
            WireRequest::MarkRead {
                chat_jid,
                through_message_id,
            } => {
                // Answered here, not from a task: the read is bounded and
                // applied synchronously, and the receipts leave on their own.
                let outcome = self.mark_read(client, &chat_jid, through_message_id.as_deref());
                let result = match outcome {
                    CommandOutcome::Accepted => Ok(DaemonResponse::Ack),
                    CommandOutcome::NoSession(detail) => Err(ApiError::not_connected(detail)),
                    CommandOutcome::Refused(detail) => Err(ApiError::invalid_argument(detail)),
                    CommandOutcome::Busy(_) => Err(ApiError::too_busy()),
                };
                send_wire_result(&answer_to, id, result);
                let _ = reply.send(CommandOutcome::Accepted);
                drop(permit);
            }
            WireRequest::MarkUnread { chat_jid } => {
                let task = client.mark_chat_unread(chat_jid);
                run_wire(answer_to.clone(), id, reply, permit, task, |()| {
                    DaemonResponse::Ack
                });
            }
            WireRequest::PinChat { chat_jid, pin } => {
                let task = client.pin_chat(chat_jid, pin);
                run_wire(answer_to.clone(), id, reply, permit, task, |()| {
                    DaemonResponse::Ack
                });
            }
            WireRequest::MuteChat {
                chat_jid,
                mute_duration_seconds,
            } => {
                let task = client.mute_chat(chat_jid, mute_duration_seconds);
                run_wire(answer_to.clone(), id, reply, permit, task, |()| {
                    DaemonResponse::Ack
                });
            }
            WireRequest::ArchiveChat { chat_jid, archive } => {
                let task = client.archive_chat(chat_jid, archive);
                run_wire(answer_to.clone(), id, reply, permit, task, |()| {
                    DaemonResponse::Ack
                });
            }
            WireRequest::SetPresence { chat_jid, state } => match state {
                PresenceState::Composing => {
                    if let Some(chat) = chat_jid {
                        client.send_composing(&chat);
                    }
                    send_wire_result(&answer_to, id, Ok(DaemonResponse::Ack));
                    let _ = reply.send(CommandOutcome::Accepted);
                    drop(permit);
                }
                PresenceState::Paused => {
                    if let Some(chat) = chat_jid {
                        client.send_paused(&chat);
                    }
                    send_wire_result(&answer_to, id, Ok(DaemonResponse::Ack));
                    let _ = reply.send(CommandOutcome::Accepted);
                    drop(permit);
                }
                PresenceState::Recording => {
                    if let Some(chat) = chat_jid {
                        client.send_recording(&chat);
                    }
                    send_wire_result(&answer_to, id, Ok(DaemonResponse::Ack));
                    let _ = reply.send(CommandOutcome::Accepted);
                    drop(permit);
                }
                PresenceState::Available => {
                    let task = client.set_available();
                    run_wire(answer_to.clone(), id, reply, permit, task, |()| {
                        DaemonResponse::Ack
                    });
                }
                PresenceState::Unavailable => {
                    let task = client.set_unavailable();
                    run_wire(answer_to.clone(), id, reply, permit, task, |()| {
                        DaemonResponse::Ack
                    });
                }
            },
            WireRequest::ListCalls { limit } => {
                let calls = self.calls.list(limit);
                let answer_to = answer_to.clone();
                oxidezap_session::spawn(async move {
                    send_wire_result(&answer_to, id, Ok(DaemonResponse::Calls { calls }));
                    let _ = reply.send(CommandOutcome::Accepted);
                    drop(permit);
                });
            }
            WireRequest::EditMessage {
                chat_jid,
                message_id,
                new_text,
            } => {
                let task = client.edit_message(chat_jid, message_id, new_text);
                run_wire(answer_to.clone(), id, reply, permit, task, |sent_id| {
                    DaemonResponse::MessageSent {
                        id: sent_id,
                        timestamp_ms: wacore::time::now_millis(),
                        enqueued: false,
                    }
                });
            }
            WireRequest::RevokeMessage {
                chat_jid,
                message_id,
                for_everyone,
            } => {
                let task = client.revoke_message(chat_jid, message_id, for_everyone);
                run_wire(answer_to.clone(), id, reply, permit, task, |()| {
                    DaemonResponse::Ack
                });
            }
            WireRequest::ForwardMessage {
                source_chat_jid,
                message_id,
                target_chat_jid,
            } => {
                let task = client.forward_message(source_chat_jid, message_id, target_chat_jid);
                run_wire(answer_to.clone(), id, reply, permit, task, |sent_id| {
                    DaemonResponse::MessageSent {
                        id: sent_id,
                        timestamp_ms: wacore::time::now_millis(),
                        enqueued: false,
                    }
                });
            }
            WireRequest::SendPoll {
                to,
                question,
                options,
                selectable_count,
            } => {
                let task = client.create_poll(to, question, options, selectable_count);
                run_wire(
                    answer_to.clone(),
                    id,
                    reply,
                    permit,
                    task,
                    |(sent_id, ts)| DaemonResponse::MessageSent {
                        id: sent_id,
                        timestamp_ms: ts,
                        enqueued: false,
                    },
                );
            }
            WireRequest::VotePoll {
                chat_jid,
                poll_id,
                selected_option_indices,
            } => {
                let task = client.vote_poll(chat_jid, poll_id, selected_option_indices);
                run_wire(answer_to.clone(), id, reply, permit, task, |()| {
                    DaemonResponse::Ack
                });
            }
            WireRequest::ListPolls { chat_jid, limit } => {
                let task = client.list_polls(chat_jid, limit as i64);
                run_wire(answer_to.clone(), id, reply, permit, task, |polls| {
                    DaemonResponse::Polls {
                        polls: polls.into_iter().map(poll_view_to_dto).collect(),
                    }
                });
            }
            WireRequest::GetPoll { chat_jid, poll_id } => {
                let task = client.get_poll(chat_jid, poll_id);
                run_wire_opt(
                    answer_to.clone(),
                    id,
                    reply,
                    permit,
                    task,
                    |poll| DaemonResponse::Poll(poll_view_to_dto(poll)),
                    "no such poll",
                );
            }
            WireRequest::SendLocation {
                to,
                latitude,
                longitude,
                name,
            } => {
                let task = client.send_location(to, latitude, longitude, name);
                run_wire(
                    answer_to.clone(),
                    id,
                    reply,
                    permit,
                    task,
                    |(sent_id, ts)| DaemonResponse::MessageSent {
                        id: sent_id,
                        timestamp_ms: ts,
                        enqueued: false,
                    },
                );
            }
            WireRequest::SendStatus { text } => {
                let task = client.send_status(text);
                run_wire(
                    answer_to.clone(),
                    id,
                    reply,
                    permit,
                    task,
                    |(sent_id, ts)| DaemonResponse::MessageSent {
                        id: sent_id,
                        timestamp_ms: ts,
                        enqueued: false,
                    },
                );
            }
            WireRequest::SendSticker { to, file_path } => {
                let task = client.send_sticker_from_path(to, file_path);
                run_wire(
                    answer_to.clone(),
                    id,
                    reply,
                    permit,
                    task,
                    |(sent_id, ts)| DaemonResponse::MessageSent {
                        id: sent_id,
                        timestamp_ms: ts,
                        enqueued: false,
                    },
                );
            }
            WireRequest::SendListResponse {
                to,
                title,
                row_id,
                reply_to,
            } => {
                let task = client.send_list_response(to, title, row_id, reply_to);
                run_wire(answer_to.clone(), id, reply, permit, task, |sent_id| {
                    DaemonResponse::MessageSent {
                        id: sent_id,
                        timestamp_ms: wacore::time::now_millis(),
                        enqueued: false,
                    }
                });
            }
            WireRequest::ListStarredMessages { limit } => {
                let task = client.list_starred(limit as i64);
                run_wire(answer_to.clone(), id, reply, permit, task, |starred| {
                    DaemonResponse::Messages {
                        messages: starred
                            .into_iter()
                            .map(|(chat_jid, message)| chat_message_to_dto(&chat_jid, message))
                            .collect(),
                        next_cursor: None,
                    }
                });
            }
            WireRequest::ListGroups => {
                let task = client.list_groups();
                run_wire(answer_to.clone(), id, reply, permit, task, |groups| {
                    DaemonResponse::Groups {
                        groups: groups.into_iter().map(group_entry_to_dto).collect(),
                    }
                });
            }
            WireRequest::GetGroupInfo { group_jid } => {
                let task = client.get_group_info(group_jid);
                run_wire(answer_to.clone(), id, reply, permit, task, |info| {
                    DaemonResponse::Group(group_details_to_dto(info))
                });
            }
            WireRequest::CreateGroup {
                subject,
                participants,
            } => {
                let task = client.create_group(subject, participants);
                run_wire(answer_to.clone(), id, reply, permit, task, |jid| {
                    DaemonResponse::GroupCreated { jid }
                });
            }
            WireRequest::SetGroupTopic { group_jid, topic } => {
                let task = client.set_group_topic(group_jid, topic);
                run_wire(answer_to.clone(), id, reply, permit, task, |()| {
                    DaemonResponse::Ack
                });
            }
            WireRequest::SetGroupDescription {
                group_jid,
                description,
            } => {
                let task = client.set_group_description(group_jid, description);
                run_wire(answer_to.clone(), id, reply, permit, task, |()| {
                    DaemonResponse::Ack
                });
            }
            WireRequest::ManageGroupParticipant {
                group_jid,
                participant_jid,
                action,
            } => {
                let action = match action {
                    WireParticipantAction::Add => oxidezap_session::ParticipantChange::Add,
                    WireParticipantAction::Remove => oxidezap_session::ParticipantChange::Remove,
                    WireParticipantAction::Promote => oxidezap_session::ParticipantChange::Promote,
                    WireParticipantAction::Demote => oxidezap_session::ParticipantChange::Demote,
                };
                let task = client.manage_group_participant(group_jid, participant_jid, action);
                run_wire(answer_to.clone(), id, reply, permit, task, |_| {
                    DaemonResponse::Ack
                });
            }
            WireRequest::GetGroupInviteLink { group_jid, reset } => {
                let task = client.get_group_invite_link(group_jid, reset);
                run_wire(answer_to.clone(), id, reply, permit, task, |link| {
                    DaemonResponse::GroupInviteLink { link }
                });
            }
            WireRequest::JoinGroup { invite_code } => {
                let task = client.join_group(invite_code);
                run_wire(answer_to.clone(), id, reply, permit, task, |joined| {
                    DaemonResponse::GroupJoined {
                        jid: joined.jid,
                        pending_approval: joined.pending_approval,
                    }
                });
            }
            WireRequest::LeaveGroup { group_jid } => {
                let task = client.leave_group(group_jid);
                run_wire(answer_to.clone(), id, reply, permit, task, |()| {
                    DaemonResponse::Ack
                });
            }
            WireRequest::SetGroupPermissions {
                group_jid,
                announce_only,
                locked,
            } => {
                let task = client.set_group_permissions(group_jid, announce_only, locked);
                run_wire(answer_to.clone(), id, reply, permit, task, |()| {
                    DaemonResponse::Ack
                });
            }
            WireRequest::ListGroupJoinRequests { group_jid } => {
                let task = client.list_group_join_requests(group_jid);
                run_wire(answer_to.clone(), id, reply, permit, task, |requests| {
                    DaemonResponse::GroupJoinRequests {
                        requests: requests
                            .into_iter()
                            .map(|r| GroupJoinRequestDto {
                                jid: r.jid,
                                request_time_secs: r.request_time_secs,
                            })
                            .collect(),
                    }
                });
            }
            WireRequest::ManageGroupJoinRequest {
                group_jid,
                participant_jid,
                approve,
            } => {
                let task = client.manage_group_join_request(group_jid, participant_jid, approve);
                run_wire(answer_to.clone(), id, reply, permit, task, |()| {
                    DaemonResponse::Ack
                });
            }
            WireRequest::ListChannels => {
                let task = client.list_channels();
                run_wire(answer_to.clone(), id, reply, permit, task, |channels| {
                    DaemonResponse::Channels {
                        channels: channels.into_iter().map(channel_view_to_dto).collect(),
                    }
                });
            }
            WireRequest::GetChannelInfo { channel_jid } => {
                let task = client.get_channel(channel_jid);
                run_wire(answer_to.clone(), id, reply, permit, task, |channel| {
                    DaemonResponse::Channel(channel_view_to_dto(channel))
                });
            }
            WireRequest::JoinChannel { channel_jid } => {
                let task = client.join_channel(channel_jid);
                run_wire(answer_to.clone(), id, reply, permit, task, |channel| {
                    DaemonResponse::Channel(channel_view_to_dto(channel))
                });
            }
            WireRequest::LeaveChannel { channel_jid } => {
                let task = client.leave_channel(channel_jid);
                run_wire(answer_to.clone(), id, reply, permit, task, |()| {
                    DaemonResponse::Ack
                });
            }
            WireRequest::GetProfile { jid } => {
                let task = client.get_profile(jid);
                run_wire(answer_to.clone(), id, reply, permit, task, |profile| {
                    DaemonResponse::Profile(profile_view_to_dto(profile))
                });
            }
            WireRequest::GetBusinessProfile { jid } => {
                let task = client.get_business_profile(jid);
                run_wire(answer_to.clone(), id, reply, permit, task, |profile| {
                    DaemonResponse::Profile(profile_view_to_dto(profile))
                });
            }
            WireRequest::SetProfileAbout { about } => {
                let task = client.set_profile_about(about);
                run_wire(answer_to.clone(), id, reply, permit, task, |()| {
                    DaemonResponse::Ack
                });
            }
            WireRequest::SetProfileName { name } => {
                let task = client.set_profile_name(name);
                run_wire(answer_to.clone(), id, reply, permit, task, |()| {
                    DaemonResponse::Ack
                });
            }
            WireRequest::SetProfilePicture { file_path } => {
                let task = client.set_profile_picture_from_path(file_path);
                run_wire(answer_to.clone(), id, reply, permit, task, |()| {
                    DaemonResponse::Ack
                });
            }
            WireRequest::RemoveProfilePicture => {
                let task = client.remove_profile_picture();
                run_wire(answer_to.clone(), id, reply, permit, task, |()| {
                    DaemonResponse::Ack
                });
            }
            WireRequest::CheckContact { phone } => {
                let task = client.check_contact(phone);
                run_wire(answer_to.clone(), id, reply, permit, task, |checked| {
                    DaemonResponse::ContactCheck {
                        phone: checked.phone,
                        is_registered: checked.is_registered,
                        jid: checked.jid,
                    }
                });
            }
            WireRequest::GetContact { jid } => {
                let task = client.get_contact(jid);
                run_wire_opt(
                    answer_to.clone(),
                    id,
                    reply,
                    permit,
                    task,
                    |contact| DaemonResponse::Contact(contact_to_dto(contact)),
                    "no such contact",
                );
            }
            WireRequest::RefreshContacts { jid } => {
                let task = client.refresh_contacts_and_read(jid);
                run_wire(answer_to.clone(), id, reply, permit, task, |contacts| {
                    DaemonResponse::Contacts {
                        contacts: contacts.into_iter().map(contact_to_dto).collect(),
                    }
                });
            }
            WireRequest::SetContactAlias { jid, alias } => {
                let task = client.set_contact_alias(jid, alias);
                run_wire_opt(
                    answer_to.clone(),
                    id,
                    reply,
                    permit,
                    task,
                    |contact| DaemonResponse::Contact(contact_to_dto(contact)),
                    "no such contact",
                );
            }
            WireRequest::TagContact { jid, tag } => {
                let task = client.tag_contact(jid, tag);
                run_wire_opt(
                    answer_to.clone(),
                    id,
                    reply,
                    permit,
                    task,
                    |contact| DaemonResponse::Contact(contact_to_dto(contact)),
                    "no such contact",
                );
            }
            WireRequest::UntagContact { jid, tag } => {
                let task = client.untag_contact(jid, tag);
                run_wire_opt(
                    answer_to.clone(),
                    id,
                    reply,
                    permit,
                    task,
                    |contact| DaemonResponse::Contact(contact_to_dto(contact)),
                    "no such contact",
                );
            }
            WireRequest::DownloadMedia {
                chat_jid,
                message_id,
                destination,
            } => {
                let task = client.download_media_by_id(chat_jid, message_id.clone());
                let answer_to = answer_to.clone();
                oxidezap_session::spawn(async move {
                    let result = match task.await {
                        Ok(Ok(bytes)) => {
                            let path =
                                destination
                                    .map(std::path::PathBuf::from)
                                    .unwrap_or_else(|| {
                                        std::env::temp_dir().join(format!("oxidezap-{message_id}"))
                                    });
                            let size = bytes.len() as u64;
                            match oxidezap_session::unblock(move || {
                                std::fs::write(&path, &bytes).map(|()| path)
                            })
                            .await
                            {
                                Ok(Ok(path)) => Ok(DaemonResponse::MediaDownloaded {
                                    message_id,
                                    local_path: path.to_string_lossy().into_owned(),
                                    size_bytes: size,
                                }),
                                Ok(Err(e)) => {
                                    Err(ApiError::internal(format!("could not write file: {e}")))
                                }
                                Err(e) => Err(ApiError::internal(e.to_string())),
                            }
                        }
                        Ok(Err(e)) => Err(ApiError::internal(e)),
                        Err(e) => Err(ApiError::not_connected(e.to_string())),
                    };
                    send_wire_result(&answer_to, id, result);
                    let _ = reply.send(CommandOutcome::Accepted);
                    drop(permit);
                });
            }
            WireRequest::RetryMedia {
                chat_jid,
                message_id,
            } => {
                let task = client.retry_media(chat_jid, message_id.clone());
                run_wire(answer_to.clone(), id, reply, permit, task, |_| {
                    DaemonResponse::MediaRetryRequested { message_id }
                });
            }
            WireRequest::HistoryCoverage { chat_jid } => {
                let resume = chat_jid.clone().unwrap_or_default();
                let task = client.history_coverage(chat_jid);
                run_wire(answer_to.clone(), id, reply, permit, task, |coverage| {
                    DaemonResponse::HistoryCoverage {
                        chat_jid: resume,
                        oldest_ts: coverage.oldest_ms,
                        newest_ts: coverage.newest_ms,
                        stored_count: coverage.stored_count,
                    }
                });
            }
            WireRequest::HistoryBackfill { chat_jid, count } => {
                let resume = chat_jid.clone();
                let task = client.history_backfill(chat_jid, i64::from(count));
                run_wire(answer_to.clone(), id, reply, permit, task, |coverage| {
                    DaemonResponse::HistoryCoverage {
                        chat_jid: resume,
                        oldest_ts: coverage.oldest_ms,
                        newest_ts: coverage.newest_ms,
                        stored_count: coverage.stored_count,
                    }
                });
            }
            WireRequest::RequestPairCode { phone } => {
                // The session publishes the `PairCode` event and hands the
                // code back here, so a script that asked synchronously gets
                // the code rather than having to subscribe for it.
                let task = client.request_pair_code(phone);
                let answer_to = answer_to.clone();
                oxidezap_session::spawn(async move {
                    let result = match task.await {
                        Ok(Ok(pair)) => Ok(DaemonResponse::PairCode {
                            code: pair.code,
                            expires_at_ms: pair.expires_at_ms,
                        }),
                        Ok(Err(e)) => Err(ApiError::invalid_argument(e)),
                        Err(e) => Err(ApiError::not_connected(e.to_string())),
                    };
                    send_wire_result(&answer_to, id, result);
                    let _ = reply.send(CommandOutcome::Accepted);
                    drop(permit);
                });
            }
            WireRequest::BackfillMedia { chat_jid, limit } => {
                let task = client.backfill_media(chat_jid, limit.clamp(1, 200) as i64);
                let answer_to = answer_to.clone();
                // Account-scoped, like every other durable key: an unscoped
                // write would let this account's backfilled bytes be served as
                // another's and would survive this account's wipe.
                let account_media = crate::media::AccountMedia::new(self.hub.account_id());
                oxidezap_session::spawn(async move {
                    let result = match task.await {
                        Ok(Ok(report)) => {
                            let mut downloaded = 0u64;
                            for file in &report.files {
                                // `download_key` is the local, content-addressed
                                // name; `AccountMedia` applies the account
                                // prefix on the way in and expects it stripped
                                // on the way to `has`.
                                let Some(local) = crate::media::download_key(&file.file_enc_sha256)
                                else {
                                    continue;
                                };
                                if account_media.has(&account_media.key(&local)) {
                                    continue;
                                }
                                if account_media.put_owned(&local, file.bytes.clone()).is_ok() {
                                    downloaded += 1;
                                }
                            }
                            Ok(DaemonResponse::MediaBackfilled {
                                requested: report.requested,
                                downloaded,
                            })
                        }
                        Ok(Err(e)) => Err(ApiError::internal(e)),
                        Err(e) => Err(ApiError::not_connected(e.to_string())),
                    };
                    send_wire_result(&answer_to, id, result);
                    let _ = reply.send(CommandOutcome::Accepted);
                    drop(permit);
                });
            }
            WireRequest::CleanupChats => {
                let task = client.cleanup_chats();
                run_wire(answer_to.clone(), id, reply, permit, task, |removed| {
                    DaemonResponse::ChatsCleaned { removed }
                });
            }
            WireRequest::PurgeMessages { chat_jid } => {
                let task = client.purge_messages(chat_jid);
                run_wire(answer_to.clone(), id, reply, permit, task, |purged| {
                    DaemonResponse::MessagesPurged { purged }
                });
            }
            _ => {
                let answer_to = answer_to.clone();
                oxidezap_session::spawn(async move {
                    send_wire_result(
                        &answer_to,
                        id,
                        Err(ApiError::unsupported("wire action not supported in bridge")),
                    );
                    let _ = reply.send(CommandOutcome::Accepted);
                    drop(permit);
                });
            }
        }
    }

    async fn act(&mut self, client: &WhatsAppClient, action: Action) -> CommandOutcome {
        // Checked again here, not only where the request arrived. The account
        // can drop in the window between the two, and the session's own answer
        // to a command it cannot carry out is a log line the requester never
        // sees.
        let connection = self.hub.connection();
        if action.needs_network() && !connection.is_connected() {
            // The same un-drawing the server does one layer up, because this
            // check exists for the window between the two: a call the asking
            // window has already drawn must not be left to disappear on the
            // next snapshot, which is what a front end writes down as an
            // attempt nobody answered.
            if let Action::Call(CallAction::Start { placeholder_id, .. }) = &action {
                self.hub
                    .calls(|calls| calls.mark_unrecorded(placeholder_id));
                self.hub.republish_calls();
            }
            return CommandOutcome::NoSession(format!("not connected: {connection:?}"));
        }

        match action {
            Action::Wire { .. } => unreachable!("Wire actions are handled in begin_slow"),
            Action::SendText(oxidezap_ipc::SendText {
                jid,
                text,
                local_id,
                quoted,
            }) => {
                let Some(permit) = self.permit() else {
                    return too_busy();
                };
                // The id is the client's when it has one: it drew the message
                // before it was sent and cannot match the rename otherwise.
                // The daemon makes one up for a client that draws nothing — it
                // still has to be unique, because a collision would rename the
                // wrong send.
                hold(
                    permit,
                    [client.send_message(
                        &jid,
                        &text,
                        local_id.unwrap_or_else(next_local_id),
                        quoted,
                    )],
                );
                CommandOutcome::Accepted
            }
            Action::SendReaction(oxidezap_ipc::SendReaction {
                chat_jid,
                message_id,
                emoji,
            }) => {
                let Some(permit) = self.permit() else {
                    return too_busy();
                };
                // Nothing to draw ahead of the send: the front end paints
                // its own reaction optimistically and the network echo
                // confirms it, so the task's answer is dropped like a send's
                // own delivery is — `hold` only keeps the permit for it.
                let task = client.send_reaction(chat_jid, message_id, emoji);
                hold(
                    permit,
                    [oxidezap_session::spawn(async move {
                        let _ = task.await;
                    })],
                );
                CommandOutcome::Accepted
            }
            Action::SendAudio(oxidezap_ipc::SendAudio {
                jid,
                upload,
                duration_secs,
                waveform,
                local_id,
                quoted,
            }) => {
                // The permit first, and the payload only once it is held.
                // See `too_busy`: taking the bytes first made a refusal
                // destructive, and the retry it asks for could then only be
                // refused again for having nothing to send.
                let Some(permit) = self.permit() else {
                    return too_busy();
                };
                // Through the cache, not the socket: a voice note is the one
                // thing a client sends that is too big for a frame. Taken
                // rather than read: the client wrote it directly, so its bytes
                // never counted toward the cache's own sweep and nothing else
                // would ever remove it. Account-scoped, so a send can only
                // ever consume a payload this same account staged.
                let Some(audio) =
                    crate::media::AccountMedia::new(self.hub.account_id()).take_staged(&upload)
                else {
                    return CommandOutcome::Refused(format!(
                        "no audio cached for this account under {upload}; write it before sending"
                    ));
                };
                hold(
                    permit,
                    [client.send_audio_message(
                        &jid,
                        audio,
                        duration_secs,
                        waveform,
                        local_id.unwrap_or_else(next_local_id),
                        quoted,
                    )],
                );
                CommandOutcome::Accepted
            }
            Action::SendMedia(oxidezap_ipc::SendMedia {
                jid,
                upload,
                kind,
                mime_type,
                file_name,
                caption,
                local_id,
                quoted,
            }) => {
                // Through the cache, exactly as a voice note is, and taken
                // rather than read for the same reason: the client wrote it
                // directly, so its bytes never counted toward the cache's own
                // sweep and nothing else would ever remove it.
                //
                // Off this thread, unlike a voice note, and the difference is
                // the size: natively this is a `read` and a `remove_file` of
                // whatever was staged, up to the protocol's whole ceiling, and
                // a blocking read of sixty-four megabytes on a runtime worker
                // stops far more than this loop. `unblock` is the seam that
                // answers for both platforms — a thread pool on the desktop,
                // and a call in a page, where the cache is a map in memory and
                // there is neither a file to read nor anywhere to hand it.
                //
                // The permit is taken first, before anything is consumed:
                // see `too_busy`.
                let Some(permit) = self.permit() else {
                    return too_busy();
                };
                let account_media = crate::media::AccountMedia::new(self.hub.account_id());
                let taken = {
                    let upload = upload.clone();
                    oxidezap_session::unblock(move || account_media.take_staged(&upload)).await
                };
                // Two answers, not one. A read that never ran — the worker
                // panicked, or the runtime is going down — is not a payload
                // nobody staged, and folding the two told a client that had
                // just staged a file to stage it before sending.
                let data = match taken {
                    Ok(Some(data)) => data,
                    Ok(None) => {
                        return CommandOutcome::Refused(format!(
                            "nothing cached under {upload}; stage it before sending"
                        ));
                    }
                    Err(e) => {
                        return CommandOutcome::Refused(format!(
                            "the payload staged under {upload} could not be read: {e}"
                        ));
                    }
                };
                hold(
                    permit,
                    [client.send_media_message(
                        &jid,
                        oxidezap_session::OutgoingFile {
                            data,
                            kind,
                            mime_type,
                            file_name,
                            caption,
                        },
                        local_id.unwrap_or_else(next_local_id),
                        quoted,
                    )],
                );
                CommandOutcome::Accepted
            }
            Action::MarkRead(oxidezap_ipc::MarkRead {
                jid,
                through_message_id,
            }) => self.mark_read(client, &jid, through_message_id.as_deref()),
            // Answered from a task of its own. See `begin_slow`.
            Action::MarkStatusWatched(_)
            | Action::LoadMessages { .. }
            | Action::LoadChats { .. }
            | Action::GroupMembers { .. } => {
                CommandOutcome::Refused("a store read reached the wrong path".to_string())
            }
            Action::Typing(oxidezap_ipc::Typing { jid, composing }) => {
                if composing {
                    client.send_composing(&jid);
                } else {
                    client.send_paused(&jid);
                }
                CommandOutcome::Accepted
            }
            Action::VotePoll(oxidezap_ipc::VotePoll {
                chat_jid,
                poll_id,
                selected_option_indices,
            }) => {
                // Off this loop like a wire vote: the ballot encrypts to
                // the creation's secret and the network round trip is not
                // this loop's work. A failure is a log line — the bubble
                // draws the options, not the outcome, and there is no
                // staged send to un-draw.
                let task = client.vote_poll(chat_jid, poll_id, selected_option_indices);
                oxidezap_session::spawn(async move {
                    if let Err(detail) = task.await {
                        log::warn!("poll vote failed: {detail}");
                    }
                });
                CommandOutcome::Accepted
            }
            // The daemon mirrors what the caller just did to its own call
            // state, because a call placed here is not an event anybody could
            // replay: `OutgoingCallStarted` only renames one that already
            // exists. A window that attaches mid-call is served this.
            Action::Call(action) => {
                match action {
                    CallAction::Start {
                        jid,
                        video,
                        placeholder_id,
                    } => {
                        // Refused here, not merely in the window that asked.
                        // Two front ends can both pass their own `is_busy`
                        // check before either has seen the other's state, and
                        // `set_outgoing` replaces the stage — which left the
                        // first call running with no UI anywhere and no way to
                        // hang it up. The daemon owns the session and the
                        // audio devices, so its state is the one that decides.
                        if self.hub.call_state().is_busy() {
                            // Refusing is not enough on its own. The window
                            // that asked drew its own outgoing call before
                            // asking — it passed its copy of the state before
                            // this one moved — and the refusal rides no
                            // request id, so nothing on that side connects it
                            // back to the stage it drew. Marking the call
                            // unrecorded stops it being written into the
                            // conversation as an attempt that was never made,
                            // and republishing is what clears it from screen.
                            self.hub
                                .calls(|calls| calls.mark_unrecorded(&placeholder_id));
                            self.hub.republish_calls();
                            return CommandOutcome::Refused(
                                "a call is already up; end it before placing another".to_string(),
                            );
                        }
                        // The name off the chat list, the same place a front
                        // end would look.
                        let name = self
                            .hub
                            .chat(&jid)
                            .map_or_else(|| jid.clone(), |chat| chat.name);
                        self.hub.calls(|calls| {
                            calls.set_outgoing(oxidezap_core::OutgoingCall::new(
                                placeholder_id.clone(),
                                jid.clone(),
                                name,
                                video,
                            ));
                        });
                        client.start_call(&jid, video, placeholder_id);
                    }
                    // Answering brings the media up here, so the call is live
                    // from this moment: waiting for an event that only fires
                    // for the *other* direction left the daemon publishing a
                    // ringing offer over a connected call.
                    CallAction::Accept { call_id } => {
                        // Answering a video call answers it with video: the
                        // offer said what kind of call this is, and the
                        // camera has to be attached before the accept goes
                        // out. A window that wants to answer with the camera
                        // off turns it off once the call is up, which is what
                        // a phone does too.
                        //
                        // One question and not two: read separately, a front
                        // end whose frame is older than this state accepted a
                        // call the stage no longer holds, so nothing here
                        // changed, no window was told anything, and the audio
                        // came up anyway — as voice, since the kind was read
                        // off a stage that had moved on.
                        let mut with_video = None;
                        self.hub.calls(|calls| {
                            with_video = calls.accept(&call_id);
                        });
                        let Some(with_video) = with_video else {
                            return CommandOutcome::Refused(format!(
                                "no call is ringing under {call_id}"
                            ));
                        };
                        client.accept_call(&call_id, with_video);
                    }
                    // A decline is the one ending only the declining side
                    // knows about. Every other window watches the same stage
                    // disappear and, with nothing said, writes it down as
                    // missed — an unread badge and a "call back" prompt for a
                    // call its owner had just refused. Said in the state
                    // frame, so it cannot arrive after the record it prevents.
                    CallAction::Decline { call_id } => {
                        self.hub.calls(|calls| {
                            calls.end(&call_id);
                            calls.mark_ended_as(&call_id, CallOutcome::Declined);
                        });
                        client.decline_call(&call_id);
                    }
                    // `end`, not `dismiss_outgoing`: hanging up is the same
                    // gesture whatever stage the call is in, and matching only
                    // the outgoing stage left a *connected* call in the
                    // daemon's state — which it then published straight back
                    // to the front end that had just ended it.
                    CallAction::Cancel { call_id } => {
                        self.hub.calls(|calls| {
                            calls.end(&call_id);
                        });
                        client.cancel_call(&call_id);
                    }
                    CallAction::SetMuted { call_id, muted } => {
                        self.hub.calls(|calls| {
                            calls.set_muted(&call_id, muted);
                        });
                        client.set_call_muted(&call_id, muted);
                    }
                    // Not mirrored optimistically, unlike mute: opening a
                    // camera can fail and takes long enough to notice, and a
                    // state that said the camera was on before the device
                    // agreed would be published to every other window too.
                    // The session announces what the device actually did.
                    CallAction::SetVideo { call_id, enabled } => {
                        client.set_call_video(&call_id, enabled);
                    }
                    CallAction::RequestVideoKeyframe { call_id, stream } => {
                        client.request_call_video_keyframe(&call_id, stream);
                    }
                }
                CommandOutcome::Accepted
            }
            Action::Download {
                id,
                request: oxidezap_ipc::Download { media },
                answer_to,
            } => self.download(client, id, *media, answer_to),
            Action::ReloadHistory => {
                client.reload_history();
                CommandOutcome::Accepted
            }
            Action::RefreshAvatars => {
                client.reset_avatar_cache();
                CommandOutcome::Accepted
            }
            Action::EnsureAvatars(items) => {
                client.ensure_avatars(items);
                CommandOutcome::Accepted
            }
            Action::RefreshVideo => {
                // A window is drawing again — or for the first time. The gate
                // opens before the keyframe is asked for, so the frame that
                // answers the ask has somewhere to go.
                client.set_video_publishing(true);
                client.request_video_keyframe();
                CommandOutcome::Accepted
            }
            // Deferred rather than done here, because the file to purge is
            // the one the session still has open. The event loop already ends
            // by disconnecting and closing SQLite; the storage call belongs
            // after that, and reusing that path is what makes the ordering
            // hold for both a reset and a removal.
            Action::ForgetSession(disposition) => {
                // Said out loud, because somebody else has to hear it: on a
                // page a front end reconnects the instant it sends this, and
                // whatever answers must not be the session that is leaving.
                // `set_disposition` marks stopping itself; the first call
                // wins, and its answer is what says whether this one is that
                // call or one the runtime is already committed against.
                match self.lifecycle.set_disposition(disposition) {
                    super::DispositionOutcome::Recorded
                    | super::DispositionOutcome::AlreadySame => CommandOutcome::Accepted,
                    // Not `Busy`: nothing about timing will change this, and a
                    // retry is answered the same way. Refused, with the reason
                    // the caller needs to understand it lost to a different
                    // operation that is already under way.
                    super::DispositionOutcome::Conflict => CommandOutcome::Refused(format!(
                        "this account is already stopping for a different reason than {disposition:?}"
                    )),
                }
            }
        }
    }

    /// Fetch media and answer the connection that asked.
    ///
    /// Answered out of band because it takes seconds: holding the command's
    /// own reply open would stop that connection reading anything else, and a
    /// front end scrolling a chat asks for several at once.
    fn download(
        &self,
        client: &WhatsAppClient,
        id: RequestId,
        media: oxidezap_core::DownloadableMedia,
        answer_to: Outbox,
    ) -> CommandOutcome {
        // No content to address by. Refused rather than filed under a key
        // every such request would share, which answered one message's
        // download with another's bytes.
        let account_media = crate::media::AccountMedia::new(self.hub.account_id());
        let Some(local_key) = crate::media::download_key(&media.file_enc_sha256) else {
            answer_now(
                &answer_to,
                downloaded(
                    id,
                    // Refused rather than failed: this one *is* about the
                    // request. The media named in it carries nothing to fetch
                    // it by, and asking again with the same media asks the
                    // same impossible question.
                    Err(ProtocolError::Refused {
                        detail: "that media carries no content hash to fetch it by".to_string(),
                    }),
                ),
            );
            return CommandOutcome::Accepted;
        };
        // Already here: the same media shared into two chats, or a front end
        // that restarted. No network, no permit, no wait.
        //
        // Claimed rather than asked about, because the next line promises it:
        // an entry nothing is holding can be swept between this answer and
        // the front end reading it. See `media::claim`.
        let key = account_media.key(&local_key);
        if account_media.claim(&key) {
            answer_now(&answer_to, downloaded(id, Ok(key)));
            return CommandOutcome::Accepted;
        }

        let Some(permit) = self.permit() else {
            return too_busy();
        };
        let bytes = client.download_downloadable_media(media);
        oxidezap_session::spawn(async move {
            let result = finish_download(bytes.await, |bytes| {
                account_media.put_owned(&local_key, bytes)
            });
            // The same rule as a page: an answer nobody delivered leaves the
            // asker waiting on it forever. See `answer_now`.
            answer_now(&answer_to, downloaded(id, result));
            drop(permit);
        });
        CommandOutcome::Accepted
    }

    /// Mark a chat read, no further than the requester has actually seen.
    fn mark_read(
        &mut self,
        client: &WhatsAppClient,
        jid: &str,
        through_message_id: Option<&str>,
    ) -> CommandOutcome {
        let (boundary, read) = match self.read_plan(jid, through_message_id) {
            Ok(plan) => plan,
            Err(reason) => return CommandOutcome::Refused(reason),
        };

        let Some(permit) = self.permit() else {
            return too_busy();
        };
        // Receipts turn the sender's ticks blue; the bounded chat action
        // persists the read across devices without swallowing anything newer.
        // Both are what the GUI does on opening a chat.
        hold(
            permit,
            [
                client.send_read_receipts(jid, self.reads().take_receipts(jid)),
                client.mark_chat_read(jid, boundary),
            ],
        );

        // Locally, now. The store's reloader debounces on a quiet window, so
        // waiting for it would leave the badge up for as long as the account
        // stays busy — exactly when a user is most likely to be clearing it.
        // And remembered, so the reload that is already in flight for the
        // message that raised the badge cannot put it straight back.
        self.reads().record_read(jid, read);
        if let Some(mut summary) = self.hub.chat(jid).filter(ChatSummary::has_unread) {
            summary.unread = 0;
            summary.manually_unread = false;
            self.hub
                .apply(Change::live(DaemonEvent::ChatUpdated(summary)));
        }
        CommandOutcome::Accepted
    }

    /// How far a read for `jid` may go, or why it may not run at all.
    ///
    /// Returns the boundary to hand the session and what to remember as read.
    /// Separate from carrying it out because every way this can go wrong ends
    /// in a message a person has to be able to act on.
    pub(super) fn read_plan(
        &self,
        jid: &str,
        through_message_id: Option<&str>,
    ) -> Result<(Option<ReadBoundary>, ReadRecord), String> {
        let Some(summary) = self.hub.chat(jid) else {
            return Err(format!("no such chat: {}", observe_str(jid)));
        };

        match self.reads().boundary(jid) {
            Some((secs, ids)) => {
                // What the requester says it is looking at, against the second
                // this read would clear. A read is irreversible and clears
                // *whole seconds*, so a client that has fallen behind — because
                // a message arrived, or because another client is further along
                // — must catch up rather than have the daemon consume arrivals
                // on its behalf. Naming a message from an older second fails
                // here, which is the guard.
                //
                // Membership, not equality with the daemon's own newest.
                // WhatsApp stamps to the second, so a burst arrives with
                // identical timestamps and the two sides order those siblings
                // by whatever their storage did — the store's row order here,
                // `(timestamp, id)` in a front end. Demanding the *same* last
                // message meant a chat that had ever received two messages in
                // one second could never be marked read again by anyone: every
                // request was refused, the badge came back on the next
                // hydration, and the advice in the refusal ("take a snapshot
                // and ask again") could not be followed, because asking again
                // produced the same id. Every id at `secs` is one this read
                // covers, so any of them is an honest claim to have seen it.
                let seen =
                    through_message_id.is_some_and(|id| ids.iter().any(|(known, ..)| known == id));
                if !seen {
                    return Err(format!(
                        "{} holds messages the preview you saw does not cover; \
                         take a snapshot and ask again",
                        observe_str(jid)
                    ));
                }
                let read = ReadRecord::through(secs, &ids);
                Ok((Some((secs, ids)), read))
            }
            // Nothing to bound, because there is nothing behind it. Refusing
            // this would make a chat marked unread by hand impossible to
            // clear.
            None if summary.last_message.is_none() => Ok((None, ReadRecord::nothing_behind_it())),
            None => Err(format!(
                "no message boundary known for {}; let its history load before marking it read",
                observe_str(jid)
            )),
        }
    }

    pub(super) fn permit(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.in_flight).try_acquire_owned().ok()
    }
}

/// Keep a permit until the work it paid for is over.
///
/// The session's calls spawn and return, so the permit cannot be released
/// where it was taken; a task that outlives this one holds it until every
/// handle has resolved. `JoinHandle` errors are the session's runtime going
/// away, which is a shutdown, not something to report.
fn hold<const N: usize>(permit: OwnedSemaphorePermit, work: [oxidezap_session::Task<()>; N]) {
    oxidezap_session::spawn(async move {
        for handle in work {
            let _ = handle.await;
        }
        drop(permit);
    });
}

/// The answer a command gets when there is no permit for it.
///
/// It asks the client to try again, which is a promise about the state this
/// refusal leaves behind: nothing the retry would need may have been consumed
/// on the way to it. A send used to take its staged payload out of the cache
/// *before* asking for a permit — `media::take` removes the only copy — so a
/// front end told "retry shortly" retried a send whose bytes no longer
/// existed and was answered "stage it before sending", for a file it had
/// staged. Every caller therefore takes the permit before it takes anything
/// else.
fn too_busy() -> CommandOutcome {
    CommandOutcome::Busy(format!(
        "{MAX_IN_FLIGHT} operations are already in flight; retry shortly"
    ))
}

/// Hand one answer to the connection that asked for it, without dropping it
/// and without parking this task.
///
/// A connection's outbox is bounded, and a request answered into a full one
/// is a request that is never answered at all: the front end keeps it in
/// `pending` and the view that asked keeps waiting, so a page nobody delivered
/// is a list that never asks again. Nothing here may block on that queue
/// either — the caller is the bridge, and the whole session waits on it — so a
/// full outbox is handed to a task that waits on the connection's own writer.
/// The frame is dropped only when the connection itself is gone, which is the
/// one case where there is nobody left to tell.
///
/// Delivery is therefore by id and not by order: a frame parked here can be
/// overtaken by the next one if that one fits, so two pages of one paged
/// `LoadMessages` can arrive the wrong way round. Every answer names its
/// `RequestId` and a front end matches on it, so nothing is lost. Serializing
/// would mean a spill queue per connection, which buys an ordering nothing
/// reads at the cost of a second buffer per client.
fn answer_now(answer_to: &Outbox, frame: String) {
    use tokio::sync::mpsc::error::TrySendError;
    if let Err(TrySendError::Full(frame)) = answer_to.try_send(frame) {
        let outbox = answer_to.clone();
        oxidezap_session::spawn(async move {
            let _ = outbox.send(frame).await;
        });
    }
}

/// What a finished download is worth to whoever asked: the key its bytes were
/// cached under, or why not — in terms that side can act on.
///
/// Three failures that were one string. A front end can do nothing with
/// "refused" but show it, and the only question it actually has is whether
/// asking again could work: over the network, yes; against a full disk, no,
/// and the second download would fail exactly as the first did. The session
/// going away is neither — there is nothing to retry against until it comes
/// back — and that is what [`ProtocolError::NoSession`] already means
/// everywhere else in this daemon.
///
/// `store` is the write rather than a call to it, so the one failure that
/// cannot be provoked from a test — a disk with nothing left on it — is still
/// the failure this classifies.
fn finish_download(
    fetched: Result<Result<Vec<u8>, String>, tokio::sync::oneshot::error::RecvError>,
    store: impl FnOnce(Vec<u8>) -> anyhow::Result<String>,
) -> Result<String, ProtocolError> {
    match fetched {
        // The bytes arrived and the cache would not take them. Nothing about
        // that is the network's, and nothing about it is the client's: a full
        // disk, a directory this process may not write into, a cache path
        // that does not resolve. None of them is answered by downloading the
        // same media again. `{e:#}` rather than `{e}` because the context
        // chain is where the actual reason is — the bare error is "renaming
        // into /…/d-ab12", and its source is the one that says why.
        Ok(Ok(bytes)) => store(bytes).map_err(|e| ProtocolError::Failed {
            detail: format!("the download could not be cached: {e:#}"),
            retryable: false,
        }),
        // The network, or the server, or the media having expired on it. The
        // first two are worth another go and the third is not, but nothing
        // this side holds can tell them apart, and a download that is offered
        // again costs one tap.
        Ok(Err(detail)) => Err(ProtocolError::Failed {
            detail,
            retryable: true,
        }),
        // The session went away mid-download: its sender was dropped with it.
        Err(_) => Err(ProtocolError::NoSession {
            detail: "the session stopped before the download finished".to_string(),
        }),
    }
}

/// The answer to a download, whichever way it went.
///
/// Success names the cache key; failure is the same error frame every other
/// request gets, under the same id.
fn downloaded(id: RequestId, result: Result<String, ProtocolError>) -> String {
    let frame = match result {
        Ok(key) => DaemonMessage::Downloaded { id, key },
        Err(error) => DaemonMessage::Error {
            id: Some(id),
            error,
        },
    };
    // Neither shape can fail to serialize; spelling the fallback out beats an
    // unwrap in a spawned task.
    serde_json::to_string(&frame)
        .unwrap_or_else(|e| format!(r#"{{"type":"error","error":"malformed","detail":"{e}"}}"#))
}

/// One answer to a request that carried its own result.
///
/// Success is the frame; failure is the error frame every other request gets,
/// under the same id. Neither shape can fail to serialize; spelling the
/// fallback out beats an unwrap in a spawned task.
fn answered(id: RequestId, result: Result<DaemonMessage, ProtocolError>) -> String {
    let frame = result.unwrap_or_else(|error| DaemonMessage::Error {
        id: Some(id),
        error,
    });
    serde_json::to_string(&frame)
        .unwrap_or_else(|e| format!(r#"{{"type":"error","error":"malformed","detail":"{e}"}}"#))
}

/// Unique optimistic-send id.
///
/// A millisecond timestamp alone collides on fast double-sends, and the
/// session renames the bubble by this id when the server assigns the real
/// one, so a collision would rename the wrong message.
fn next_local_id() -> String {
    use portable_atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "daemon_{}_{}",
        wacore::time::now_millis(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Answer a wire request from the session task that ran it.
///
/// One shape for every awaited mutation: the session's string failure becomes
/// an internal error, a dead executor a dropped connection, and the value the
/// response the caller maps it to. The permit is held until the work it paid
/// for is over, like every other slow action.
fn run_wire<T>(
    answer_to: Outbox,
    id: u64,
    reply: tokio::sync::oneshot::Sender<CommandOutcome>,
    permit: OwnedSemaphorePermit,
    task: impl std::future::Future<Output = Result<Result<T, String>, impl std::fmt::Display>>
    + Send
    + 'static,
    ok: impl FnOnce(T) -> DaemonResponse + Send + 'static,
) {
    oxidezap_session::spawn(async move {
        let result = match task.await {
            Ok(Ok(value)) => Ok(ok(value)),
            Ok(Err(e)) => Err(ApiError::internal(e)),
            Err(e) => Err(ApiError::not_connected(e.to_string())),
        };
        send_wire_result(&answer_to, id, result);
        let _ = reply.send(CommandOutcome::Accepted);
        drop(permit);
    });
}

/// [`run_wire`], for lookups that can miss: `None` is a not-found error,
// not an empty answer.
#[allow(clippy::too_many_arguments)]
fn run_wire_opt<T>(
    answer_to: Outbox,
    id: u64,
    reply: tokio::sync::oneshot::Sender<CommandOutcome>,
    permit: OwnedSemaphorePermit,
    task: impl std::future::Future<Output = Result<Result<Option<T>, String>, impl std::fmt::Display>>
    + Send
    + 'static,
    ok: impl FnOnce(T) -> DaemonResponse + Send + 'static,
    missing: &'static str,
) {
    oxidezap_session::spawn(async move {
        let result = match task.await {
            Ok(Ok(Some(value))) => Ok(ok(value)),
            Ok(Ok(None)) => Err(ApiError::not_found(missing)),
            Ok(Err(e)) => Err(ApiError::internal(e)),
            Err(e) => Err(ApiError::not_connected(e.to_string())),
        };
        send_wire_result(&answer_to, id, result);
        let _ = reply.send(CommandOutcome::Accepted);
        drop(permit);
    });
}

/// A session contact view as the protocol's contact.
fn contact_to_dto(contact: oxidezap_session::ContactView) -> ContactDto {
    ContactDto {
        jid: contact.jid,
        name: contact.name,
        push_name: contact.push_name,
        phone: contact.phone,
        is_business: contact.is_business,
        alias: contact.alias,
        tags: contact.tags,
    }
}

/// A group-list row as the protocol's group.
fn group_entry_to_dto(group: oxidezap_session::GroupListEntry) -> GroupDto {
    GroupDto {
        jid: group.jid,
        subject: group.subject,
        description: None,
        owner_jid: None,
        participant_count: group.participant_count,
        announce_only: false,
        locked: false,
        participants: Vec::new(),
    }
}

/// Full group metadata as the protocol's group.
fn group_details_to_dto(group: oxidezap_session::GroupDetails) -> GroupDto {
    GroupDto {
        jid: group.jid,
        subject: group.subject,
        description: group.description,
        owner_jid: group.owner_jid,
        participant_count: group.participant_count,
        announce_only: group.announce_only,
        locked: group.locked,
        participants: group
            .participants
            .into_iter()
            .map(|p| GroupParticipantDto {
                jid: p.jid,
                is_admin: p.is_admin,
                is_superadmin: p.is_superadmin,
            })
            .collect(),
    }
}

/// A stored poll as the protocol's poll. Tallies are not decrypted here —
/// see the session's note — so every option starts at zero.
fn poll_view_to_dto(poll: oxidezap_session::PollView) -> PollDto {
    PollDto {
        id: poll.id,
        chat_jid: poll.chat_jid,
        question: poll.question,
        options: poll
            .options
            .into_iter()
            .enumerate()
            .map(|(index, name)| PollOptionDto {
                index: index as u32,
                name,
                vote_count: 0,
                voters: Vec::new(),
            })
            .collect(),
        selectable_count: poll.selectable_count,
    }
}

/// A subscribed channel as the protocol's channel.
fn channel_view_to_dto(channel: oxidezap_session::ChannelView) -> ChannelDto {
    ChannelDto {
        jid: channel.jid,
        name: channel.name,
        description: channel.description,
        subscriber_count: channel.subscriber_count,
        picture_url: channel.picture_url,
    }
}

/// A profile as the protocol's profile.
fn profile_view_to_dto(profile: oxidezap_session::ProfileView) -> oxidezap_wire::dto::ProfileDto {
    oxidezap_wire::dto::ProfileDto {
        jid: profile.jid,
        name: profile.name,
        about: profile.about,
        picture_url: profile.picture_url,
    }
}

fn send_wire_result(answer_to: &Outbox, id: u64, result: Result<DaemonResponse, ApiError>) {
    let envelope = match result {
        Ok(data) => ResponseEnvelope::success(id, data),
        Err(err) => ResponseEnvelope::error(Some(id), err),
    };
    if let Ok(line) = serde_json::to_string(&envelope) {
        answer_now(answer_to, line);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use oxidezap_core::{OutgoingMedia, UiEvent, fixtures};
    use oxidezap_ipc::{DaemonMessage, ProtocolError};
    use oxidezap_session::WhatsAppClient;

    use super::super::tests::{bridge, loaded, message, received, saturate, stored_chat};
    use super::*;

    /// A session that has never been started.
    ///
    /// [`WhatsAppClient::new`] builds the executor and nothing else — it opens
    /// no store and reaches no network — so it is exactly what these tests
    /// need: the thing `act` hands work to, with no account behind it. It has
    /// to be let go of through [`WhatsAppClient::close`] rather than dropped,
    /// because it owns a Tokio runtime and tokio refuses to drop one inside an
    /// async context.
    fn client() -> WhatsAppClient {
        WhatsAppClient::new().expect("an executor")
    }

    /// A bridge that believes it is connected.
    ///
    /// Everything that takes a permit also needs the network, and `act`
    /// refuses those outright when the hub says the account is unreachable —
    /// so without this every test below would be answered `NoSession` before
    /// it reached the line it is about.
    fn connected() -> Bridge {
        let mut bridge = bridge();
        bridge.observe(UiEvent::Connected);
        bridge
    }

    /// A key nothing else in this process is using.
    ///
    /// The media cache is one directory shared by everything running as this
    /// user, tests in other crates included, so a fixed key would have two
    /// tests writing over each other's payload.
    fn staged_key(what: &str) -> String {
        use portable_atomic::AtomicU64;
        static SEQ: AtomicU64 = AtomicU64::new(0);
        // Under the account the test bridge serves (`StateHub::new` is the
        // legacy one): a send consumes only a payload its own account staged.
        oxidezap_ipc::account_staged_key(
            oxidezap_core::AccountId::LEGACY,
            &format!(
                "act-test-{what}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
        )
    }

    fn send_media(upload: &str) -> Action {
        Action::SendMedia(oxidezap_ipc::SendMedia {
            jid: fixtures::PEER.to_string(),
            upload: upload.to_string(),
            kind: OutgoingMedia::Image,
            mime_type: "image/jpeg".to_string(),
            file_name: "foto.jpg".to_string(),
            caption: None,
            local_id: Some("local-1".to_string()),
            quoted: None,
        })
    }

    fn send_audio(upload: &str) -> Action {
        Action::SendAudio(oxidezap_ipc::SendAudio {
            jid: fixtures::PEER.to_string(),
            upload: upload.to_string(),
            duration_secs: 3,
            waveform: vec![1, 2, 3],
            local_id: Some("local-2".to_string()),
            quoted: None,
        })
    }

    fn send_text() -> Action {
        Action::SendText(oxidezap_ipc::SendText {
            jid: fixtures::PEER.to_string(),
            text: "oi".to_string(),
            local_id: None,
            quoted: None,
        })
    }

    fn send_reaction() -> Action {
        Action::SendReaction(oxidezap_ipc::SendReaction {
            chat_jid: fixtures::PEER.to_string(),
            message_id: "3EB0A".to_string(),
            emoji: "👍".to_string(),
        })
    }

    /// The refusal for being busy asks the client to retry, so it must not
    /// have spent what the retry would need. `media::take` removes the only
    /// copy of a staged payload: taking it before the permit meant "retry
    /// shortly" destroyed the file, and the retry was answered "stage it
    /// before sending" for a file that had just been staged.
    #[tokio::test]
    async fn a_busy_daemon_refuses_a_file_send_without_eating_the_file() {
        let key = staged_key("media");
        crate::media::put(&key, b"os bytes que alguem quer enviar").expect("a staged payload");

        let mut bridge = connected();
        let client = client();
        let held = saturate(&bridge);

        let outcome = bridge.act(&client, send_media(&key)).await;
        assert!(
            matches!(&outcome, CommandOutcome::Busy(detail) if detail.contains("in flight")),
            "expected a busy answer, got {outcome:?}"
        );
        assert!(
            crate::media::has(&key),
            "the busy answer ate the payload it asked the client to send again"
        );

        // And the retry the refusal asked for goes through, which is the
        // whole of what the promise is worth.
        drop(held);
        assert_eq!(
            bridge.act(&client, send_media(&key)).await,
            CommandOutcome::Accepted
        );
        assert!(
            !crate::media::has(&key),
            "an accepted send takes the payload with it"
        );
        client.close(Duration::from_secs(1)).await;
    }

    /// The same rule for a voice note, which takes its payload on this very
    /// thread rather than off it.
    #[tokio::test]
    async fn a_busy_daemon_refuses_a_voice_note_without_eating_the_recording() {
        let key = staged_key("audio");
        crate::media::put(&key, b"uma gravacao").expect("a staged payload");

        let mut bridge = connected();
        let client = client();
        let held = saturate(&bridge);

        let outcome = bridge.act(&client, send_audio(&key)).await;
        assert!(
            matches!(&outcome, CommandOutcome::Busy(detail) if detail.contains("in flight")),
            "expected a busy answer, got {outcome:?}"
        );
        assert!(crate::media::has(&key), "the busy answer ate the recording");

        drop(held);
        assert_eq!(
            bridge.act(&client, send_audio(&key)).await,
            CommandOutcome::Accepted
        );
        client.close(Duration::from_secs(1)).await;
    }

    /// A send naming a key nothing was staged under is a different answer
    /// from a busy daemon, and it says what to do about it.
    #[tokio::test]
    async fn a_send_naming_nothing_staged_is_told_to_stage_it() {
        let mut bridge = connected();
        let client = client();
        let key = staged_key("absent");

        let outcome = bridge.act(&client, send_media(&key)).await;
        assert!(
            matches!(&outcome, CommandOutcome::Refused(detail) if detail.contains("stage it")),
            "got {outcome:?}"
        );
        client.close(Duration::from_secs(1)).await;
    }

    /// The permit decides whether the work runs at all, so a command that
    /// cannot get one is refused rather than queued behind work nobody can
    /// see.
    #[tokio::test]
    async fn a_command_that_cannot_get_a_permit_is_refused() {
        let mut bridge = connected();
        let client = client();
        let _held = saturate(&bridge);

        assert_eq!(bridge.act(&client, send_text()).await, too_busy());
        client.close(Duration::from_secs(1)).await;
    }

    /// A reaction holds a permit like a send does: a busy refusal — not a
    /// queued send — when none is free, and accepted once one is.
    #[tokio::test]
    async fn a_reaction_without_a_free_permit_is_refused() {
        let mut bridge = connected();
        let client = client();
        let held = saturate(&bridge);

        assert_eq!(bridge.act(&client, send_reaction()).await, too_busy());

        drop(held);
        assert_eq!(
            bridge.act(&client, send_reaction()).await,
            CommandOutcome::Accepted
        );
        client.close(Duration::from_secs(1)).await;
    }

    /// A vote is taken, not staged: there is no bubble to un-draw on
    /// refusal, so the session takes it and a ballot it cannot cast is a
    /// log line rather than an outcome. Here there is no session at all,
    /// and the answer is still acceptance — the failure has nowhere to go
    /// that the voter could act on.
    #[tokio::test]
    async fn a_vote_is_accepted_and_cast_off_the_command_path() {
        let mut bridge = connected();
        let client = client();

        let outcome = bridge
            .act(
                &client,
                Action::VotePoll(oxidezap_ipc::VotePoll {
                    chat_jid: fixtures::PEER.to_string(),
                    poll_id: "3EB0C".to_string(),
                    selected_option_indices: vec![0],
                }),
            )
            .await;
        assert_eq!(outcome, CommandOutcome::Accepted);
        client.close(Duration::from_secs(1)).await;
    }

    /// Everything that touches the account is refused while there is none,
    /// and the answer says which of the two kinds of "no" this is.
    #[tokio::test]
    async fn a_send_without_a_connection_is_no_session() {
        let mut bridge = bridge();
        let client = client();

        let outcome = bridge.act(&client, send_text()).await;
        assert!(
            matches!(outcome, CommandOutcome::NoSession(_)),
            "got {outcome:?}"
        );
        client.close(Duration::from_secs(1)).await;
    }

    /// A store read is answered from a task of its own, so reaching this arm
    /// at all is a routing mistake rather than something a client did.
    #[tokio::test]
    async fn a_store_read_that_reaches_the_command_path_is_refused() {
        let mut bridge = connected();
        let client = client();

        let outcome = bridge
            .act(
                &client,
                Action::MarkStatusWatched(oxidezap_ipc::MarkStatusWatched {
                    message_ids: vec!["3EB0".to_string()],
                }),
            )
            .await;
        assert!(
            matches!(&outcome, CommandOutcome::Refused(detail) if detail.contains("wrong path")),
            "got {outcome:?}"
        );
        client.close(Duration::from_secs(1)).await;
    }

    /// A read is bounded by what the requester saw, and a chat this side has
    /// never heard of bounds nothing.
    #[test]
    fn a_read_for_an_unknown_chat_is_refused() {
        let bridge = bridge();
        let refusal = bridge
            .read_plan(fixtures::PEER, Some("3EB0"))
            .expect_err("no such chat");
        assert!(refusal.contains("no such chat"), "{refusal}");
    }

    /// Naming a message from an older second is a requester that has fallen
    /// behind, and clearing a whole second on its word would consume the
    /// arrivals it has not seen.
    #[test]
    fn a_read_naming_a_message_the_requester_did_not_see_is_refused() {
        let mut bridge = bridge();
        bridge.observe(received(
            fixtures::PEER,
            message("m1", fixtures::PEER, 10, false, false),
            None,
        ));
        bridge.observe(received(
            fixtures::PEER,
            message("m2", fixtures::PEER, 20, false, false),
            None,
        ));

        let refusal = bridge
            .read_plan(fixtures::PEER, Some("m1"))
            .expect_err("the preview is behind");
        assert!(
            refusal.contains("take a snapshot and ask again"),
            "{refusal}"
        );

        // The newest second is an honest claim, and it is what bounds the
        // read.
        let (boundary, _) = bridge
            .read_plan(fixtures::PEER, Some("m2"))
            .expect("a plan");
        assert_eq!(boundary.expect("a boundary").0, 20);
    }

    /// A chat marked unread by hand has nothing behind it, and refusing that
    /// would leave the badge impossible to clear.
    #[test]
    fn a_chat_with_nothing_behind_it_can_still_be_cleared() {
        let mut bridge = bridge();
        bridge.observe(loaded(vec![stored_chat(fixtures::PEER, 0, vec![])]));

        let (boundary, _) = bridge.read_plan(fixtures::PEER, None).expect("a plan");
        assert!(boundary.is_none(), "there is nothing to bound");
    }

    /// The bytes arrived and the cache would not take them. Nothing about
    /// that is worth a second download: the disk is as full as it was.
    #[test]
    fn a_download_that_could_not_be_cached_is_not_worth_asking_again() {
        let answer = finish_download(Ok(Ok(vec![1, 2, 3])), |_| {
            Err(anyhow::anyhow!("No space left on device")
                .context("writing /run/oxidezap/media/w-1.0"))
        });
        match answer {
            Err(ProtocolError::Failed { detail, retryable }) => {
                assert!(!retryable, "a full disk is not fixed by downloading again");
                assert!(
                    detail.contains("No space left on device"),
                    "the reason is buried in the context chain: {detail}"
                );
            }
            other => panic!("expected a failure that cannot be retried, got {other:?}"),
        }
    }

    /// The network, on the other hand, is worth another go — and this is the
    /// distinction the front end had no way to make.
    #[test]
    fn a_download_that_failed_on_the_network_is_worth_asking_again() {
        let answer = finish_download(Ok(Err("connection reset by peer".to_string())), |_| {
            panic!("nothing was downloaded, so nothing is written")
        });
        assert_eq!(
            answer,
            Err(ProtocolError::Failed {
                detail: "connection reset by peer".to_string(),
                retryable: true,
            })
        );
    }

    /// And the third is neither: there is nothing to retry against until the
    /// account is back, which is what `NoSession` says everywhere else.
    #[test]
    fn a_session_that_went_away_mid_download_is_no_session() {
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<Vec<u8>, String>>();
        drop(tx);
        let answer = finish_download(rx.blocking_recv(), |_| panic!("nothing to write"));
        assert!(
            matches!(answer, Err(ProtocolError::NoSession { .. })),
            "got {answer:?}"
        );
    }

    /// The happy path still answers with the key the bytes were filed under,
    /// which is how the asker fetches them.
    #[test]
    fn a_download_that_landed_answers_with_its_key() {
        let answer = finish_download(Ok(Ok(vec![7])), |bytes| {
            assert_eq!(bytes, vec![7]);
            Ok("d-abc".to_string())
        });
        assert_eq!(answer, Ok("d-abc".to_string()));
    }

    /// The bit has to survive the wire, or the daemon is the only side that
    /// knows it. This is the frame a front end actually reads.
    #[test]
    fn the_frame_a_client_reads_carries_whether_to_retry() {
        let parse = |frame: &str| match serde_json::from_str::<DaemonMessage>(frame) {
            Ok(DaemonMessage::Error { error, .. }) => error,
            other => panic!("expected an error frame, got {other:?}"),
        };

        let network = parse(&downloaded(
            7,
            Err(ProtocolError::Failed {
                detail: "connection reset by peer".to_string(),
                retryable: true,
            }),
        ));
        let disk = parse(&downloaded(
            8,
            Err(ProtocolError::Failed {
                detail: "the download could not be cached: No space left on device".to_string(),
                retryable: false,
            }),
        ));

        assert_ne!(
            network, disk,
            "the two failures a client has to tell apart read identically"
        );
        assert!(matches!(
            network,
            ProtocolError::Failed {
                retryable: true,
                ..
            }
        ));
        assert!(matches!(
            disk,
            ProtocolError::Failed {
                retryable: false,
                ..
            }
        ));
    }
}
