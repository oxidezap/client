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
                    let mut cache = self.message_list_cache.borrow_mut();
                    self.chats.retain(|c| {
                        let keep = !c.is_from_store()
                            || loaded.contains(c.jid.as_str())
                            || self.selected_chat.as_deref() == Some(c.jid.as_str());
                        // The cache is keyed by JID alone, so a dropped chat
                        // whose JID is later recreated with the same message
                        // count and layout inputs would render the removed
                        // chat's messages. Message data must not cross a chat
                        // lifetime.
                        if !keep {
                            cache.remove(&c.jid);
                        }
                        keep
                    });
                }
                for chat in chats {
                    // Later loads (post-HistorySync re-hydration) fold into
                    // chats the UI already shows instead of being dropped.
                    match self.chats.iter_mut().find(|c| c.jid == chat.jid) {
                        Some(existing) => {
                            let jid = chat.jid.clone();
                            existing.merge_history(chat);
                            // The open chat was read locally the moment the
                            // message arrived; the store row commits with the
                            // unread bump before our receipt lands, so the
                            // hydrated counter must not resurrect the badge.
                            if self.selected_chat.as_deref() == Some(jid.as_str()) {
                                existing.mark_as_read();
                            }
                            self.invalidate_message_cache(&jid);
                        }
                        None => self.chats.push(chat),
                    }
                }
                self.chats
                    .sort_by_key(|c| std::cmp::Reverse(c.last_message_time));
                // Count-based cache guards can't see reordering/merges.
                self.invalidate_chat_cache();
                cx.notify();
            }
            UiEvent::QrCode { code, timeout_secs } => {
                let pair_code = match &self.app_state {
                    AppState::WaitingForPairing { pair_code, .. } => pair_code.clone(),
                    _ => None,
                };
                let cached_qr = generate_qr_png(&code).map(|png_bytes| CachedQrCode {
                    data: code,
                    png_bytes: Arc::new(png_bytes),
                });
                self.app_state = AppState::WaitingForPairing {
                    qr_code: cached_qr,
                    pair_code,
                    timeout_secs,
                };
                cx.notify();
            }
            UiEvent::PairCode { code, timeout_secs } => {
                let qr_code = match &self.app_state {
                    AppState::WaitingForPairing { qr_code, .. } => qr_code.clone(),
                    _ => None,
                };
                self.app_state = AppState::WaitingForPairing {
                    qr_code,
                    pair_code: Some(code),
                    timeout_secs,
                };
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
                self.app_state = AppState::LoggedOut { message };
                cx.notify();
            }
            UiEvent::Disconnected(reason) => {
                self.app_state = AppState::Error(reason);
                cx.notify();
            }
            UiEvent::Error(msg) => {
                self.app_state = AppState::Error(msg);
                cx.notify();
            }
            UiEvent::MessageReceived {
                chat_jid,
                message,
                sender_name,
            } => {
                self.handle_message_received(chat_jid, *message, sender_name);
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
                }
                self.invalidate_message_cache(&chat_jid);
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
                    && let Some(msg) = chat.messages.iter_mut().find(|m| m.id == message_id)
                {
                    msg.failed = true;
                    self.invalidate_message_cache(&chat_jid);
                    cx.notify();
                }
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
            UiEvent::IncomingCall(mut call) => {
                if let Some(name) = self
                    .find_chat(&call.caller_jid)
                    .map(|chat| chat.name.clone())
                {
                    call.caller_name = name;
                }
                info!(
                    "Incoming {} call from {}",
                    if call.is_video { "video" } else { "audio" },
                    observe_str(&call.caller_jid)
                );
                self.call_state.set_incoming(call);
                cx.notify();
            }
            UiEvent::CallAccepted(call_id) => {
                info!("Call {} accepted by peer", call_id);
                // Dismiss the incoming call popup if it matches
                let incoming_dismissed = self.call_state.dismiss_incoming(&call_id);
                // For outgoing calls, transition to Connected state
                let outgoing_connected = self.call_state.set_outgoing_connected(&call_id);
                if outgoing_connected {
                    info!("Outgoing call {} is now connected", call_id);
                }
                if incoming_dismissed || outgoing_connected {
                    cx.notify();
                }
            }
            UiEvent::CallEnded(call_id) => {
                info!("Call {} ended", call_id);
                // Dismiss the incoming call popup if it matches
                let incoming_dismissed = self.call_state.dismiss_incoming(&call_id);
                // Also dismiss outgoing call if it matches
                let outgoing_dismissed = self.call_state.dismiss_outgoing(&call_id);
                if incoming_dismissed || outgoing_dismissed {
                    cx.notify();
                }
            }
            UiEvent::OutgoingCallStarted {
                call_id,
                recipient_jid,
            } => {
                info!(
                    "Outgoing call started: {} to {}",
                    call_id,
                    observe_str(&recipient_jid)
                );
                // Update the outgoing call with the actual call ID from CallManager
                if self
                    .call_state
                    .update_outgoing_call_id(&recipient_jid, call_id.clone())
                {
                    cx.notify();
                } else {
                    // Popup already dismissed: the user cancelled while the
                    // call was connecting; hang up the now-real call.
                    if let Some(client) = &self.client {
                        client.cancel_call(&call_id);
                    }
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
                // Dismiss the outgoing call popup
                if self
                    .call_state
                    .dismiss_outgoing_for_recipient(&recipient_jid)
                {
                    cx.notify();
                }
            }
        }
    }
}
