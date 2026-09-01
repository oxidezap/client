//! The session's event stream becoming daemon state.
//!
//! Folding an event does not touch the client: what the session has to be told
//! back is returned as an [`Answer`] for the run loop to perform, which is what
//! lets every rule below be tested without opening a store.

use std::collections::HashSet;

use oxidezap_core::{Chat, ChatMessage, UiEvent};
use oxidezap_ipc::{
    ChatSummary, ConnectionState, DaemonEvent, DaemonMessage, MessagePreview, PairingCode,
};
use wacore_binary::jid::{Jid, JidExt};

use super::Bridge;
use super::read_tracker::ReadTracker;
use crate::state::Change;

/// What the session has to be told after an event was folded into daemon
/// state.
///
/// A return value rather than a client call inside [`Bridge::observe`], so
/// folding stays a pure function of the event and the state — which is what
/// lets it be tested without opening a store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Answer {
    Nothing,
    /// An offer with nowhere to go. Nothing on this side holds its id, so no
    /// front end can be asked to refuse it.
    Decline(oxidezap_core::CallId),
}

impl Bridge {
    /// Fold one session event into daemon state, and say what the session has
    /// to be told back.
    ///
    /// Folding does not touch the client, so this stays testable without a
    /// store: what it cannot do itself it returns, and the run loop performs.
    pub(super) fn observe(&mut self, event: UiEvent) -> Answer {
        let mut answer = Answer::Nothing;
        // Before anything is published, so a `MarkRead` that arrives right
        // behind a message already covers it. What it answers is whether the
        // message is new to this side, which is what the badge below counts.
        let first_sighting = self.reads().observe(&event);

        if let Some(frame) = passthrough(&event) {
            self.hub.signal(&frame);
        }

        // Calls are state, not just events: see `StateSnapshot::calls`. The
        // same transitions the front end applies, from the same type, so the
        // two cannot drift.
        match &event {
            UiEvent::IncomingCall(call) => {
                // A second offer during a live call is parked rather than
                // dropped; the front end draws it as a refusable strip. A
                // third has nowhere to go, and a caller nothing on this side
                // holds an id for would ring until they gave up — so it is
                // refused here, where the session is.
                let mut admission = oxidezap_core::Admission::Ringing;
                self.hub.calls(|s| admission = s.set_incoming(call.clone()));
                if admission == oxidezap_core::Admission::Refused {
                    answer = Answer::Decline(call.call_id.clone());
                }
            }
            UiEvent::OutgoingCallStarted {
                call_id,
                recipient_jid: _,
                placeholder_id,
                is_video,
            } => {
                // Renamed *and* advanced: the placeholder id was ours, and the
                // server answering with the real one is also what says the
                // call has started ringing at the far end. Leaving the state
                // to say "calling…" for the rest of the call was a difference
                // between the daemon and a front end that applied both.
                //
                // Matched on the placeholder, not the recipient: see
                // `CallState::update_outgoing_call_id`.
                let mut adopted = false;
                self.hub.calls(|s| {
                    adopted = s.update_outgoing_call_id(placeholder_id, call_id.clone(), *is_video);
                    if adopted {
                        s.set_outgoing_ringing(call_id);
                    }
                });
            }
            // Answered, from either side: the call becomes live rather than
            // being cleared. A front end attaching now has to find a call in
            // progress, not an empty state over running audio.
            UiEvent::CallAccepted(id) => self.hub.calls(|s| {
                s.connect(id);
            }),
            // The kind an incoming call was answered as, which the accept
            // decides and the offer only proposed: a camera that would not
            // open answers a video offer as a voice call. Nothing is
            // published when it agrees with the offer, which is the ordinary
            // case.
            UiEvent::CallAnswered { call_id, is_video } => self.hub.calls(|s| {
                s.answered_as(call_id, *is_video);
            }),
            UiEvent::CallEnded(id) => self.hub.calls(|s| {
                s.end(id);
            }),
            // The session correcting a mute the peer was never told about.
            // The front end drew what it asked for; this is what the device
            // is actually doing. Nothing is published when the two agree, so
            // the ordinary mute costs no frame.
            UiEvent::CallVideoChanged {
                call_id,
                stream,
                on,
            } => self.hub.calls(|s| {
                s.set_video(call_id, *stream, *on);
            }),
            // A question the peer asked, kept as state rather than left as an
            // event: a window that attaches after it was asked never saw it,
            // and would draw an ordinary camera button while somebody waited
            // on it.
            UiEvent::CallVideoRequested { call_id, pending } => self.hub.calls(|s| {
                s.set_video_requested(call_id, *pending);
            }),
            UiEvent::CallMuteChanged { call_id, muted } => self.hub.calls(|s| {
                s.set_muted(call_id, *muted);
            }),
            // The same removal, and one more thing said about it: the front
            // end writes the conversation's call record off the stage it was
            // holding, and an incoming stage that simply vanishes reads as
            // missed.
            UiEvent::CallEndedElsewhere(id) => self.hub.calls(|s| {
                s.end_elsewhere(id);
            }),
            // Marked before it is ended, because ending is what publishes the
            // removal and the explanation has to be in that same frame — a
            // reason arriving after the record it was meant to change is no
            // reason at all.
            UiEvent::CallUnrecorded(id) => self.hub.calls(|s| {
                s.mark_unrecorded(id);
                s.end(id);
            }),
            // The connection the calls run over is gone, and nothing else
            // says so: no session event ends a call when the socket dies, so
            // the stage stood, `is_busy` went on refusing every new call after
            // the reconnect, and the only way out was a cancel naming an id no
            // attached window still had.
            UiEvent::Disconnected(_) | UiEvent::LoggedOut(_) | UiEvent::Error(_) => {
                self.hub.calls(|s| {
                    s.end_all();
                });
                // And everything keyed to the account leaves with it. An
                // account reset is a departure: a snapshot taken after the
                // next pairing would otherwise open with the old identity, the
                // old chat list, and a stage the new account's front end reads
                // as a call that just ended — writing the previous account's
                // call into this one's history.
                if matches!(event, UiEvent::LoggedOut(_)) {
                    self.hub.forget_account();
                    self.reads().forget_all();
                }
            }
            UiEvent::AccountUpdated { name, jid, lid } => {
                self.hub.set_account(oxidezap_ipc::AccountIdentity {
                    name: name.clone(),
                    jid: jid.clone(),
                    lid: lid.clone(),
                });
            }
            // The stage empties for good here — nothing is about to replace
            // a call that never got placed — so a second caller parked behind
            // it comes forward, the same as when a call ends.
            UiEvent::OutgoingCallFailed { recipient_jid, .. } => self.hub.calls(|s| {
                s.fail_outgoing_to(recipient_jid);
            }),
            _ => {}
        }

        // Cloned before `translate` consumes it, queued after the hub has it.
        // A front end reacts to what it is told the instant it is told — a
        // message arriving in the open chat is marked read immediately — and
        // the runtime is multithreaded, so that `MarkRead` can reach another
        // worker while this one has not applied the message yet. `read_plan`
        // would refuse it as stale, after the client had cleared its own badge
        // and with nothing to make it ask again.
        let pending = self.hub.wants_session_events().then(|| event.clone());
        // Kept for the plugins, which are told once the state below is
        // written; `translate` consumes the event. Cloned only when one of
        // them would actually be handed it — not merely when any are loaded:
        // a history load carries every chat with its messages, a receipt a
        // whole list of ids, and a message-only plugin wants none of it.
        let observed = self.plugins.wants(&event).then(|| event.clone());

        for change in self.translate(event, first_sighting) {
            // A chat that left the store owes nothing and will never be read
            // again; keeping its ids would leak one entry per deleted
            // conversation.
            if let DaemonEvent::ChatRemoved { jid } = &change.event {
                self.reads().forget(jid);
            }
            self.hub.apply(change);
        }

        // After the state, not before. A plugin is a front end that does not
        // draw, so the instinct is to hand it the event as early as the
        // daemon hears it — but a plugin runs on its own thread and may act
        // immediately, and what it acts *through* reads the state this loop
        // has just written. Told first, a plugin answering `CONNECTED` could
        // have its send refused as `NoSession` because the hub still said
        // "connecting"; and one answering a disconnect could slip a command
        // past a check that still said connected. The head start a window
        // gains is a frame, and it costs nothing.
        if let Some(observed) = &observed {
            self.plugins.observe(observed);
        }

        if let Some(event) = pending {
            // The receiver lives as long as the thread, which lives as long as
            // the daemon.
            if let Some(publish) = &self.publish {
                let _ = publish.send(event);
            }
        }

        answer
    }

    /// Map one session event onto zero or more daemon changes.
    ///
    /// Returning a list rather than an `Option` keeps the fan-out explicit: a
    /// history load is many chat updates, and a chat with a new message is one
    /// update carrying the whole summary rather than a delta the client would
    /// have to merge.
    ///
    /// `first_sighting` is the read tracker's answer to "is this message new
    /// to this side", taken before anything was published. The badge counts
    /// it rather than counting the event, or a redelivery adds two.
    fn translate(&mut self, event: UiEvent, first_sighting: bool) -> Vec<Change> {
        match event {
            UiEvent::InitComplete => vec![connection(ConnectionState::Connecting)],
            UiEvent::Connected => vec![connection(ConnectionState::Connected)],
            // Without this the QR stays on screen until `Connected` arrives,
            // which can be a visible wait: the code has already been consumed
            // and would no longer work if scanned.
            UiEvent::PairSuccess => vec![connection(ConnectionState::Syncing)],
            UiEvent::Disconnected(reason) => {
                vec![connection(ConnectionState::Disconnected { reason })]
            }
            UiEvent::LoggedOut(message) => {
                vec![connection(ConnectionState::LoggedOut { message })]
            }
            // Each credential replaces only itself. A user who asked for a
            // phone-number code while a QR was on screen has both, and either
            // may be renewed on its own clock; clearing the other would make
            // whichever arrived first vanish from every later snapshot.
            UiEvent::QrCode { code, timeout_secs } => {
                let (_, pair_code) = self.pairing_credentials();
                vec![connection(ConnectionState::Pairing {
                    qr: Some(PairingCode {
                        code,
                        expires_at_ms: deadline_ms(timeout_secs),
                    }),
                    pair_code,
                })]
            }
            // Phone-number pairing carries its code here rather than in a QR.
            // The protocol has a field for it, so dropping the event would
            // leave a front end on that flow waiting for a code that never
            // arrives.
            UiEvent::PairCode { code, timeout_secs } => {
                let (qr, _) = self.pairing_credentials();
                vec![connection(ConnectionState::Pairing {
                    qr,
                    pair_code: Some(PairingCode {
                        code,
                        expires_at_ms: deadline_ms(timeout_secs),
                    }),
                })]
            }
            // Without this the hub sits in `Connecting` forever: the session's
            // sender outlives its worker, so no disconnect follows to correct
            // it and every client waits on a state that will never change.
            UiEvent::Error(detail) => {
                vec![connection(ConnectionState::Disconnected { reason: detail })]
            }
            // Live traffic, applied directly rather than waiting for the store
            // to republish. The reloader that produces `HistoryLoaded`
            // debounces on a quiet window, so on a busy account it can stay
            // silent through an entire burst; without these the tray badge and
            // every client snapshot would freeze for exactly as long as the
            // account is active.
            UiEvent::MessageReceived {
                chat_jid,
                message,
                sender_name,
            } => {
                let mut summary = self.hub.chat(&chat_jid).unwrap_or_else(|| ChatSummary {
                    name: live_chat_name(&chat_jid, &message, sender_name),
                    jid: chat_jid.clone(),
                    unread: 0,
                    manually_unread: false,
                    last_message: None,
                });
                // Asked of the tracker rather than recomputed, so the badge
                // and the receipts this side owes cannot disagree: the same
                // event delivered twice used to add 2 while the tracker
                // recognised the duplicate and owed one receipt.
                if first_sighting {
                    summary.unread = summary.unread.saturating_add(1);
                }
                // Only when it really is the newest. Live messages are not
                // ordered: history decryption and offline catch-up deliver
                // out of order, and moving the preview *backwards* onto an
                // older message put the daemon's boundary behind what every
                // client holds — after which a bounded read named a message
                // outside it and was refused until a store reload repaired
                // the summary. Ties by id, so a same-second sibling is
                // settled the same way on both sides.
                let arrival = (message.timestamp.timestamp_millis(), message.id.as_str());
                let newer = summary.last_message.as_ref().is_none_or(|current| {
                    arrival >= (current.timestamp_ms, current.id.as_deref().unwrap_or(""))
                });
                if newer {
                    summary.last_message = Some(MessagePreview {
                        id: Some(message.id.clone()),
                        text: message.content.clone(),
                        from_me: message.is_from_me,
                        timestamp_ms: message.timestamp.timestamp_millis(),
                    });
                }
                // Live, not from the store: a chat first seen here has no row
                // yet, and a complete reload that omits it is not evidence it
                // was deleted. See `StateHub::store_backed_chat_jids`.
                vec![Change::live(DaemonEvent::ChatUpdated(summary))]
            }
            UiEvent::HistoryLoaded {
                chats,
                complete,
                // Where the *chat list* continues, which is a front end's
                // business: this side holds no list position, it holds the
                // rows. The event carries it through untouched.
                next: _,
            } => {
                let mut changes: Vec<Change> = Vec::with_capacity(chats.len() + 1);

                // A complete load is the store's whole truth, so a chat
                // missing from it was archived or deleted elsewhere. Upserting
                // only what arrived would leave that chat in every snapshot,
                // still counting toward the tray badge, with nothing to ever
                // remove it. Only store-backed chats are diffed: a chat seen
                // live and not yet written is not something this load can
                // contradict.
                if complete {
                    let loaded: HashSet<&str> = chats.iter().map(|c| c.jid.as_str()).collect();
                    changes.extend(
                        self.hub
                            .store_backed_chat_jids()
                            .into_iter()
                            .filter(|jid| !loaded.contains(jid.as_str()))
                            .map(|jid| Change::live(DaemonEvent::ChatRemoved { jid })),
                    );
                }

                let mut reads = self.reads();
                changes.extend(chats.iter().map(|chat| chat_updated(chat, &mut reads)));
                changes
            }
            _ => Vec::new(),
        }
    }

    /// The pairing credentials currently published, or none when the daemon is
    /// not in a pairing state at all.
    fn pairing_credentials(&self) -> (Option<PairingCode>, Option<PairingCode>) {
        match self.hub.connection() {
            ConnectionState::Pairing { qr, pair_code } => (qr, pair_code),
            _ => (None, None),
        }
    }
}

/// A frame that is news rather than state, if this event is one.
///
/// Kept apart from `translate` because nothing here changes what a snapshot
/// would say: these go out once, to whoever is connected, and are gone.
fn passthrough(event: &UiEvent) -> Option<DaemonMessage> {
    match event {
        // The chat is exactly as it was, one message short. Published rather
        // than swallowed, because a client that asked for a send has no other
        // way to learn it did not happen — the acknowledgement said the
        // session took the command, not that the network took the message.
        UiEvent::SendFailed {
            chat_jid, reason, ..
        } => Some(DaemonMessage::SendFailed {
            jid: chat_jid.clone(),
            reason: reason.clone(),
        }),
        _ => None,
    }
}

fn connection(state: ConnectionState) -> Change {
    Change::live(DaemonEvent::ConnectionChanged(state))
}

/// Turn the session's "expires in N seconds" into the deadline the wire
/// carries. See [`PairingCode`] for why it is absolute.
pub(super) fn deadline_ms(timeout_secs: u64) -> i64 {
    let millis = i64::try_from(timeout_secs)
        .unwrap_or(i64::MAX)
        .saturating_mul(1_000);
    wacore::time::now_millis().saturating_add(millis)
}

/// Name a chat the store has not published yet.
///
/// In a group, a broadcast list or a status broadcast the sender is a
/// participant, not the conversation, so naming the chat after them publishes
/// "Alice" for a group of forty until a reload corrects it. These are exactly
/// the chats the session itself treats as participant-keyed. The JID is a
/// worse label but an honest one, and [`oxidezap_core::fallback_chat_name`] is
/// what a front end renders it as. Outgoing messages are skipped for the same
/// reason: the sender is us.
fn live_chat_name(chat_jid: &str, message: &ChatMessage, sender_name: Option<String>) -> String {
    let names_the_chat = !message.is_from_me && !participant_keyed(chat_jid);

    names_the_chat
        .then_some(sender_name)
        .flatten()
        .unwrap_or_else(|| chat_jid.to_string())
}

/// Whether messages in this chat come from participants rather than from the
/// chat itself. Mirrors the session's own `participant_keyed_chat`.
fn participant_keyed(chat_jid: &str) -> bool {
    chat_jid
        .parse::<Jid>()
        .is_ok_and(|jid| jid.is_group() || jid.is_broadcast_list() || jid.is_status_broadcast())
}

pub(super) fn chat_updated(chat: &Chat, reads: &mut ReadTracker) -> Change {
    // Identity and authorship of the preview both come from the newest
    // hydrated message, and from the same one: hard-coding authorship would
    // render every outgoing preview as if the peer had sent it, which is
    // exactly the indicator a chat list uses to tell them apart, and an id
    // taken from anywhere else could name a message the text does not
    // describe. `None` when the chat has a preview string but no message body
    // yet, which is the honest answer rather than a guess.
    let newest = chat.messages.last();
    let from_me = newest.is_some_and(|m| m.is_from_me);
    let mut summary = ChatSummary {
        jid: chat.jid.clone(),
        name: chat.name.clone(),
        unread: chat.unread_count,
        manually_unread: chat.manually_unread,
        last_message: chat.last_message.as_ref().map(|text| MessagePreview {
            id: newest.map(|m| m.id.clone()),
            text: text.clone(),
            from_me,
            // Milliseconds on the wire: the protocol is language-agnostic and
            // a chrono type is not, so the conversion happens here rather than
            // leaking a Rust date type into the IPC surface.
            timestamp_ms: chat.last_message_time.map_or(0, |t| t.timestamp_millis()),
        }),
    };

    // The read this daemon already issued has not reached the store yet. The
    // reload was scheduled by the very message that raised the badge, so
    // republishing its count would put the badge straight back — visibly,
    // moments after an accepted read — and leave it there until the next
    // store update.
    if reads.overrides_unread(chat, !summary.has_unread()) {
        summary.unread = 0;
        summary.manually_unread = false;
    }

    Change::from_store(DaemonEvent::ChatUpdated(summary))
}
