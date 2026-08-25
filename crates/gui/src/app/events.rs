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
            UiEvent::HistoryLoaded { chats, complete } => {
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
                if complete {
                    let loaded: std::collections::HashSet<&str> =
                        chats.iter().map(|c| c.jid.as_str()).collect();
                    let selected = self.selected_chat.clone();
                    // Rebuilt from this load rather than added to: a chat that
                    // comes back — unarchived elsewhere — is in `loaded` again
                    // and is no longer owed a removal.
                    let mut departed = std::collections::HashSet::new();
                    let mut cache = self.message_list_cache.borrow_mut();
                    self.chats.retain(|c| {
                        match survives_complete_load(c, &loaded, selected.as_deref()) {
                            Survival::Keep => true,
                            // Gone from the store, but on screen. Spared now
                            // and remembered, so that leaving it finishes the
                            // removal instead of it lingering until some later
                            // reload happens to say so again.
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
                                false
                            }
                        }
                    });
                    drop(cache);
                    self.departed_chats = departed;
                }
                for chat in chats {
                    // Later loads (post-HistorySync re-hydration) fold into
                    // chats the UI already shows instead of being dropped.
                    match self.chats.iter_mut().find(|c| c.jid == chat.jid) {
                        Some(existing) => {
                            let jid = chat.jid.clone();
                            existing.merge_history(chat);
                            // The chat *on screen* was read locally the
                            // moment the message arrived; the store row
                            // commits with the unread bump before our receipt
                            // lands, so the hydrated counter must not
                            // resurrect the badge. On screen, not selected —
                            // the same distinction the live arrival makes, and
                            // for the same reason: a reload while the reader
                            // is in Status would otherwise clear the badge of
                            // a conversation nobody was looking at.
                            if self.visible_chat.as_deref() == Some(jid.as_str()) {
                                existing.mark_as_read();
                            }
                            self.invalidate_message_cache(&jid);
                        }
                        None => self.chats.push(chat),
                    }
                }
                self.chats
                    .sort_by_key(|c| std::cmp::Reverse(c.last_message_time));
                // The merge above took the store's word for every row, and
                // the store was never told which updates have been watched.
                self.restore_watched_status();
                // Count-based cache guards can't see reordering/merges.
                self.invalidate_chat_cache();
                // Whatever arrived before its conversation did. A group
                // change is announced to a window that has never seen the
                // group, and this load is what makes it placeable.
                self.flush_pending_notices(cx);
                // A status update expires on the clock with nothing arriving
                // to say so, and this is where the feed that holds one is
                // installed. Without arming it here nothing ever did: the
                // timer only re-armed itself, so the first one was never set
                // and a lapsed update kept its row, its ring and its badge
                // until some unrelated change happened to rebuild the list.
                self.ensure_status_tick(cx);
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
                let cached_qr = generate_qr_png(&code).map(|png_bytes| CachedQrCode {
                    data: code,
                    png_bytes: Arc::new(png_bytes),
                });
                self.app_state = AppState::WaitingForPairing {
                    qr_code: cached_qr
                        .map(|qr| Issued::new(qr, timeout_secs, wacore::time::now_utc())),
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
                self.leave_connected_view(cx);
                self.app_state = AppState::Error(reason);
                // The screen offers a retry; arming it is what makes the
                // countdown on that button mean something.
                self.schedule_retry(cx);
                cx.notify();
            }
            UiEvent::Error(msg) => {
                self.leave_connected_view(cx);
                self.app_state = AppState::Error(msg);
                self.schedule_retry(cx);
                cx.notify();
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
            UiEvent::CallEnded(call_id) => {
                info!("Call {call_id} ended");
            }
            UiEvent::CallEndedElsewhere(call_id) => {
                info!("Call {call_id} was handled on another device");
            }
            UiEvent::OutgoingCallStarted {
                call_id,
                recipient_jid,
                placeholder_id: _,
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
