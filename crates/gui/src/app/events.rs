//! Translation of `UiEvent`s from the session into view state.
//!
//! Kept apart from the rest of the app because it is the one place allowed to
//! mutate state in response to the outside world; everything else reacts to a
//! user action.

use super::*;

impl WhatsAppApp {
    /// Handle a single UI event
    pub(super) fn handle_event(&mut self, event: UiEvent, cx: &mut Context<Self>) {
        match event {
            UiEvent::InitComplete => {
                self.app_state = AppState::Connecting;
                // From here on there is a `theme.json` to watch, whether or
                // not this session ever reaches the pairing screen.
                self.ensure_heartbeat(cx);
                cx.notify();
            }
            UiEvent::HistoryLoaded {
                chats,
                complete,
                next,
            } => {
                // Debug, not info: a store invalidation reloads history, and
                // acks and receipts make several of those per message.
                debug!(
                    "Loaded {} chats from durable history (complete: {complete})",
                    chats.len()
                );
                // Prune only against a COMPLETE load: there absence means the
                // chat was archived/deleted (possibly on another device), so
                // it must leave the UI too. A truncated load can't distinguish
                // that from a chat that merely fell past the window, so it
                // never prunes. The selected chat is spared either way so the
                // open conversation isn't yanked mid-view. Live-only chats
                // (`!from_store`) are spared too: during initial pairing the
                // store is still empty while live messages already populate
                // the UI, and an early reload (e.g. a push-name commit) sends
                // a complete-but-empty load that must not wipe them — whereas
                // a store-originated chat missing from a complete load really
                // was deleted/archived. (Skipping empty loads instead would
                // break clearing the last chat deleted on another device.)
                // Whatever else this load says, it says the store answered:
                // a list that ended before a history sync did has more behind
                // it now. Not gated on `complete`, which an account of a
                // hundred chats never is — see `reopen_finished_pages`.
                let reloaded: Vec<String> = chats.iter().map(|c| c.jid.clone()).collect();
                self.reopen_finished_pages(&reloaded);
                // And this load says where the list stands, which is the one
                // answer that beats anything inferred from it: `next` is the
                // position it stopped at, and a complete load is the whole
                // list. Applied after the reopen above, so the load's own
                // answer is the one that survives.
                self.note_chat_list_end(complete, next);
                if complete {
                    let loaded: std::collections::HashSet<&str> =
                        chats.iter().map(|c| c.jid.as_str()).collect();
                    // What the last frame drew, not what is selected: see
                    // `survives_complete_load`.
                    let visible = self.visible_chat.clone();
                    // Rebuilt from this load rather than added to: a chat that
                    // comes back — unarchived elsewhere — is in `loaded` again
                    // and is no longer owed a removal.
                    let mut departed = std::collections::HashSet::new();
                    let mut dropped: Vec<String> = Vec::new();
                    let mut cache = self.message_list_cache.borrow_mut();
                    self.chats.retain(|c| {
                        match survives_complete_load(c, &loaded, visible.as_deref()) {
                            Survival::Keep => true,
                            // Gone from the store, but on screen. Spared
                            // now and remembered, so that looking away
                            // finishes the removal instead of it lingering
                            // until some later reload happens to say so
                            // again.
                            Survival::Defer => {
                                departed.insert(c.jid.clone());
                                true
                            }
                            // The cache is keyed by JID alone, so a dropped
                            // chat whose JID is later recreated with the same
                            // message count and layout inputs would render the
                            // removed chat's messages. Message data must not
                            // cross a chat lifetime.
                            Survival::Drop => {
                                cache.remove(&c.jid);
                                dropped.push(c.jid.clone());
                                false
                            }
                        }
                    });
                    drop(cache);
                    // And where its history continued: a position that
                    // outlives its chat is one a recreated chat inherits, and
                    // a conversation that believes it has everything asks for
                    // nothing. See `forget_chat_paging`.
                    self.forget_chat_paging(&dropped);
                    self.departed_chats = departed;
                }
                // The updates this load itself brought back already read:
                // the store has caught up on those, and only those. Read off
                // the load rather than the merge, because a merge keeps the
                // row this window marked and it is indistinguishable
                // afterwards from one the store agreed about.
                let agreed: std::collections::HashSet<String> = chats
                    .iter()
                    .filter(|chat| chat.is_status)
                    .flat_map(|chat| chat.messages.iter())
                    .filter(|message| message.is_read)
                    .map(|message| message.id.clone())
                    .collect();
                self.install_chats(chats, &agreed, cx);
                cx.notify();
            }
            UiEvent::QrCode { code, timeout_secs } => {
                // The phone code keeps the deadline it was issued with. A
                // QR rotates every few seconds and a phone code lives for
                // minutes; one shared clock made each refresh of one restate
                // the other's remaining life as its own.
                let pair_code = match &self.app_state {
                    AppState::WaitingForPairing { pair_code, .. } => pair_code.clone(),
                    _ => None,
                };
                // The one that could not be drawn keeps the one before it.
                // A rotation arrives every few seconds and the previous code
                // is still scannable for a moment; replacing it with nothing
                // left "Waiting for a code…" on screen under a life bar
                // counting down over nothing at all.
                let previous = match &self.app_state {
                    AppState::WaitingForPairing { qr_code, .. } => qr_code.clone(),
                    _ => None,
                };
                let cached_qr = generate_qr_png(&code)
                    .map(|png_bytes| CachedQrCode {
                        data: code,
                        png_bytes: Arc::new(png_bytes),
                    })
                    .map(|qr| Issued::new(qr, timeout_secs, wacore::time::now_utc()))
                    .or(previous);
                self.app_state = AppState::WaitingForPairing {
                    qr_code: cached_qr,
                    pair_code,
                };
                // The countdown on that screen is read off the clock during
                // render, and nothing else repaints while it is up.
                self.ensure_heartbeat(cx);
                cx.notify();
            }
            UiEvent::PairCode { code, timeout_secs } => {
                let qr_code = match &self.app_state {
                    AppState::WaitingForPairing { qr_code, .. } => qr_code.clone(),
                    _ => None,
                };
                self.app_state = AppState::WaitingForPairing {
                    qr_code,
                    pair_code: Some(Issued::new(code, timeout_secs, wacore::time::now_utc())),
                };
                self.ensure_heartbeat(cx);
                cx.notify();
            }
            UiEvent::PairSuccess => {
                self.app_state = AppState::Syncing;
                cx.notify();
            }
            UiEvent::Connected => {
                self.app_state = AppState::Connected;
                cx.notify();
            }
            UiEvent::LoggedOut(message) => {
                self.leave_connected_view(cx);
                self.app_state = AppState::LoggedOut { message };
                cx.notify();
            }
            UiEvent::Disconnected(reason) => {
                // Nothing diagnosed it, so it is the outage the screen was
                // written for.
                self.connection_ended(oxidezap_core::Fault::unreachable(reason), cx);
            }
            UiEvent::Error(msg) => {
                self.connection_ended(oxidezap_core::Fault::unreachable(msg), cx);
            }
            UiEvent::MessageReceived {
                chat_jid,
                message,
                sender_name,
            } => {
                self.handle_message_received(chat_jid, *message, sender_name);
                // A live status update brings its own 24-hour deadline with
                // it, and it can be the earliest one on screen.
                self.ensure_status_tick(cx);
                cx.notify();
            }
            UiEvent::MessageIdAssigned {
                chat_jid,
                local_id,
                message_id,
            } => {
                if let Some(chat) = self.find_chat_mut(&chat_jid) {
                    // Re-insert, not mutate in place: messages sort by
                    // (timestamp, id) and the rename can reorder same-second
                    // siblings.
                    chat.rename_message(&local_id, &message_id);
                    // A rename, not an acknowledgement: this id is invented
                    // locally *before* the send is even awaited, so ticking
                    // it as Sent here would promise delivery for a message
                    // still queued — or about to fail. The tick comes from
                    // the store's own ServerAck, through the reload.
                }
                self.invalidate_message_cache(&chat_jid);
                self.invalidate_chat_cache();
                cx.notify();
            }
            UiEvent::SendFailed {
                chat_jid,
                message_id,
                reason,
            } => {
                warn!(
                    "Send failed for {} in {}: {}",
                    message_id,
                    observe_str(&chat_jid),
                    reason
                );
                if let Some(chat) = self.find_chat_mut(&chat_jid)
                    && chat.mark_send_failed(&message_id)
                {
                    self.invalidate_message_cache(&chat_jid);
                    self.invalidate_chat_cache();
                    cx.notify();
                }
            }
            UiEvent::ChatPresence {
                chat_jid,
                sender_jid,
                sender_name,
                composing,
            } => {
                self.handle_chat_presence(chat_jid, sender_jid, sender_name, composing, cx);
            }
            UiEvent::AccountUpdated { name, jid, lid } => {
                if self.account_name != name || self.account_jid != jid || self.account_lid != lid {
                    self.account_name = name;
                    self.account_jid = jid;
                    self.account_lid = lid;
                    // The rows say "(You)" off this, so they are stale now.
                    self.invalidate_chat_cache();
                    cx.notify();
                }
            }
            UiEvent::SystemNotice {
                chat_jid,
                notice_id,
                at,
                notice,
            } => {
                self.handle_system_notice(chat_jid, notice_id, at, notice, cx);
            }
            UiEvent::PresenceUpdated { jid, availability } => {
                self.presence.set_availability(jid, availability);
                cx.notify();
            }
            UiEvent::ReceiptReceived {
                chat_jid,
                message_ids,
                receipt_type,
            } => {
                self.handle_receipt_received(chat_jid, message_ids, receipt_type);
                cx.notify();
            }
            UiEvent::ReactionReceived {
                chat_jid,
                message_id,
                sender,
                emoji,
            } => {
                self.handle_reaction_received(chat_jid, message_id, sender, emoji);
                cx.notify();
            }
            // A call is state, and the daemon is the only thing that writes
            // it. Every transition below was already folded into the shared
            // `CallState` and published as `CallsChanged`, which arrives on
            // the state channel *before* this event — so applying it a second
            // time here is not a redundancy, it is a different answer. It read
            // its own work as a fresh offer and parked the live call's id in
            // `waiting`, drawing a strip for a caller who did not exist whose
            // Decline hung up on the one who did. These arms observe; only
            // `adopt_calls` writes.
            UiEvent::IncomingCall(call) => {
                info!(
                    "Incoming {} call from {}",
                    if call.is_video { "video" } else { "audio" },
                    observe_str(&call.caller_jid)
                );
            }
            UiEvent::CallAccepted(call_id) => {
                info!("Call {call_id} accepted by peer");
            }
            // What the call is drawn as comes from the state the daemon
            // publishes beside this; the event is what says so out loud.
            UiEvent::CallAnswered { call_id, is_video } => {
                info!(
                    "Call {call_id} answered as {}",
                    if is_video { "video" } else { "voice" }
                );
            }
            // Said out loud *and* drawn: the call ends immediately behind
            // this, so without a notice the person sees a call that appeared
            // and vanished. The reason is the library's own words, which is
            // the point — "the relay refused the answer" is something to act
            // on, and a call that silently disappears is not.
            UiEvent::CallMediaFailed { call_id, reason } => {
                warn!("Call {call_id} could not bring up media: {reason}");
                self.notify_user(
                    format!("The call could not be connected: {reason}"),
                    crate::app::notices::Tone::Problem,
                    cx,
                );
            }
            UiEvent::CallEnded(call_id) => {
                info!("Call {call_id} ended");
            }
            UiEvent::CallEndedElsewhere(call_id) => {
                info!("Call {call_id} was handled on another device");
            }
            // Nothing to draw and nothing to write down: the removal that
            // carries this also carries `Ending::Nothing`, which is what the
            // conversation reads. Logged so a refused accept is not silent to
            // whoever is reading the console.
            UiEvent::CallUnrecorded(call_id) => {
                info!("Call {call_id} left no record: it was never answered here");
            }
            // The correction, not the request: the state it names is what the
            // session found on the handle after an announcement that did not
            // go out as asked. What the window draws comes from the daemon's
            // call state, which has already been given the same value.
            UiEvent::CallMuteChanged { call_id, muted } => {
                info!(
                    "Call {call_id} microphone is {}",
                    if muted { "muted" } else { "open" }
                );
                // The answer to what this window asked for, and the last word
                // on it: what the state frames carried in the meantime was
                // the mute the daemon still held.
                self.settle_call_muted(&call_id, muted, cx);
            }
            // What the camera really is, once the daemon has opened or closed
            // it — and, unlike the mute correction, the *answer* to what this
            // window asked for. The state published beside it is what a pane
            // is drawn from, but a settle that agrees with the state changes
            // nothing and so travels alone: a camera that would not open is
            // announced off against a state that was already off, no frame
            // goes out, and a button left waiting on one stays lit for the
            // rest of the call. So the answer is taken from here.
            UiEvent::CallVideoChanged {
                call_id,
                stream,
                on,
            } => {
                info!(
                    "Call {call_id}: the {stream:?} camera is {}",
                    if on { "on" } else { "off" }
                );
                self.settle_call_video(&call_id, stream, on, cx);
            }
            // A question, and the answer is this side's camera coming on —
            // or the question being withdrawn, which is just as much news:
            // a request the peer cancelled is one no camera can still answer.
            UiEvent::CallVideoRequested { call_id, pending } => {
                info!(
                    "Call {call_id}: the peer {} video",
                    if pending {
                        "asked to add"
                    } else {
                        "is no longer asking for"
                    }
                );
                // Nothing to apply: what the window draws comes from the call
                // state the daemon publishes beside this, the same way mute
                // does. The event is what says it out loud.
            }
            UiEvent::OutgoingCallStarted {
                call_id,
                recipient_jid,
                placeholder_id: _,
                // The kind the offer went out as is state, and the daemon
                // folds it into the stage it renames; this side draws it from
                // there like everything else about the call.
                is_video: _,
            } => {
                info!(
                    "Outgoing call started: {} to {}",
                    call_id,
                    observe_str(&recipient_jid)
                );
                // The one thing that is not state: a *command*. The id we
                // cancelled with was the placeholder we invented, which the
                // session never knew, so a call the user gave up on while it
                // was still connecting is ringing at the far end with nothing
                // holding it. The daemon not tracking it is what says so.
                if !self.call_state.holds(&call_id)
                    && let Some(client) = &self.client
                {
                    info!("cancelling {call_id}, which nobody is waiting for");
                    client.cancel_call(&call_id);
                }
            }
            UiEvent::OutgoingCallFailed {
                recipient_jid,
                error,
            } => {
                warn!(
                    "Outgoing call to {} failed: {}",
                    observe_str(&recipient_jid),
                    error
                );
            }
        }
    }
}
