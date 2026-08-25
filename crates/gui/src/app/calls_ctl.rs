//! Call controls, one level above `client.voip()`.

use gpui::{Pixels, Point};

use super::*;

impl WhatsAppApp {
    pub fn call_state(&self) -> &CallState {
        &self.call_state
    }

    /// This window's placement of the card, which no other window shares.
    pub fn call_card(&self) -> &CallCard {
        &self.call_card
    }

    pub fn incoming_call(&self) -> Option<&IncomingCall> {
        self.call_state.incoming()
    }

    pub fn outgoing_call(&self) -> Option<&OutgoingCall> {
        self.call_state.outgoing()
    }

    pub fn active_call(&self) -> Option<&ActiveCall> {
        self.call_state.active()
    }

    /// Accept the incoming call.
    ///
    /// The media comes up here, so the call becomes active immediately rather
    /// than waiting for an answer event that never arrives for an inbound
    /// call. That gap is what used to leave the audio running with no UI.
    pub fn accept_call(&mut self, cx: &mut Context<Self>) {
        let Some(client) = &self.client else {
            warn!("Cannot accept call: client is unavailable");
            return;
        };
        let Some(call) = self.call_state.take_incoming() else {
            return;
        };
        info!(
            "Accepting call {} from {}",
            call.call_id,
            observe_str(&call.caller_jid)
        );
        client.accept_call(call.call_id.as_str());
        self.call_state.connect_accepted(&call);
        self.ensure_tick(cx);
        cx.notify();
    }

    /// Refuse the second call parked behind the one on screen.
    ///
    /// Its own command, reached from its own strip on the card. Folding it
    /// into `decline_call` made the *visible* Decline button refuse a caller
    /// the user could not see, and leave the ringing one ringing.
    pub fn decline_waiting_call(&mut self, cx: &mut Context<Self>) {
        let Some(waiting) = self.call_state.take_waiting() else {
            return;
        };
        info!("Declining waiting call {}", waiting.call_id());
        if let Some(client) = &self.client {
            client.decline_call(waiting.call_id().as_str());
        }
        // Written down like any other refusal. Left out, a caller the user
        // saw and refused left no trace anywhere: the strip is gone, the
        // daemon only sends the network decline, and the conversation has no
        // row saying they rang.
        self.record_call_as(
            &Stage::Incoming(waiting.into_call()),
            Some(CallOutcome::Declined),
            cx,
        );
        cx.notify();
    }

    /// Decline the incoming call the card is showing.
    pub fn decline_call(&mut self, cx: &mut Context<Self>) {
        let Some(client) = &self.client else {
            warn!("Cannot decline call: client is unavailable");
            return;
        };
        // `decline_incoming`, not `take_incoming`: refusing an offer replaces
        // it with nothing, so a caller parked behind it has to come forward.
        // Taking the stage without promoting left the optimistic state with
        // no stage at all, and the card drew neither caller until a daemon
        // update repaired it — or, if the decline never landed, not at all.
        if let Some(call) = self.call_state.decline_incoming() {
            info!(
                "Declining call {} from {}",
                call.call_id,
                observe_str(&call.caller_jid)
            );
            client.decline_call(call.call_id.as_str());
            // A refusal is not a missed call, and a local reject emits no
            // `CallEnded` to write one later — so the record is made here,
            // saying what actually happened. The mobile decline goes through
            // `hang_up`, which records it as missed; this is the outcome the
            // enum has been carrying unused.
            self.record_call_as(&Stage::Incoming(call), Some(CallOutcome::Declined), cx);
            cx.notify();
        }
    }

    /// End whatever call is up: cancel a call we placed, decline one ringing
    /// at us, or hang up a live one.
    ///
    /// One method because it is one gesture. The card decides what to *call*
    /// it — cancelling an unanswered call is not hanging up on someone — but
    /// the effect is the same and splitting it invites the two to drift.
    pub fn hang_up(&mut self, cx: &mut Context<Self>) {
        let Some(stage) = self.call_state.take() else {
            return;
        };
        let call_id = stage.call_id().to_string();
        info!("Ending call {call_id}");
        self.call_card.call_ended();
        // A ringing offer ended by this button was refused, not missed — the
        // same thing `decline_call` writes down, and the phone viewport routes
        // its Decline through here. Deriving the outcome from the stage alone
        // gave the two buttons two different histories for one gesture.
        let outcome = matches!(stage, Stage::Incoming(_)).then_some(CallOutcome::Declined);
        self.record_call_as(&stage, outcome, cx);
        if let Some(client) = &self.client {
            match &stage {
                // A ringing offer has to be rejected rather than hung up:
                // there is no live handle, and the caller should stop ringing
                // instead of waiting out the timeout.
                Stage::Incoming(_) => client.decline_call(&call_id),
                Stage::Outgoing(_) | Stage::Active(_) => client.cancel_call(&call_id),
            }
        }
        cx.notify();
    }

    /// Mute or unmute the live call.
    pub fn toggle_call_muted(&mut self, cx: &mut Context<Self>) {
        let Some(call_id) = self.call_state.active().map(|c| c.call_id.clone()) else {
            return;
        };
        let Some(muted) = self.call_state.toggle_muted() else {
            return;
        };
        if let Some(client) = &self.client {
            client.set_call_muted(&call_id, muted);
        }
        cx.notify();
    }

    pub fn set_call_minimized(&mut self, minimized: bool, cx: &mut Context<Self>) {
        self.call_card.set_minimized(minimized);
        cx.notify();
    }

    pub fn begin_call_drag(&mut self, at: Point<Pixels>) {
        self.call_card.begin_drag(at);
    }

    /// The pointer moved while dragging the card.
    ///
    /// The bounds come from the window and the card's own measured size, so
    /// they follow a resize, a density change, and the card changing shape
    /// mid-call without anything here knowing how big it is.
    pub fn drag_call_card(
        &mut self,
        at: Point<Pixels>,
        viewport: gpui::Size<Pixels>,
        inset: Pixels,
        cx: &mut Context<Self>,
    ) {
        if self.call_card.drag_to(at) {
            self.call_card.clamp_to(viewport, inset);
            cx.notify();
        }
    }

    pub fn end_call_drag(&mut self) {
        self.call_card.end_drag();
    }

    /// Start a call to the specified JID.
    pub fn start_call(&mut self, recipient_jid: String, is_video: bool, cx: &mut Context<Self>) {
        // Offline is a read-only state, and the socket outlives it: the
        // "call back" under a missed call reached the daemon from a window
        // that had stopped waiting for a connection, and drew an outgoing
        // call in a UI that says it is not connected.
        if !self.can_send() {
            warn!("Cannot start call: this window is offline");
            return;
        }
        let Some(client) = &self.client else {
            warn!("Cannot start call: client is unavailable");
            return;
        };

        // One call at a time: placing a second would leave the first with no
        // UI to end it.
        if self.call_state.is_busy() {
            warn!("A call is already in progress");
            return;
        }

        let recipient_name = self
            .find_chat(&recipient_jid)
            .map(|chat| chat.name.clone())
            .unwrap_or_else(|| "Unknown contact".to_string());

        info!(
            "Starting {} call to {}",
            if is_video { "video" } else { "audio" },
            observe_str(&recipient_jid)
        );

        let placeholder_call_id = format!("ui-call-{}", whatsapp_rust::wacore::time::now_millis());
        let call = OutgoingCall::new(
            placeholder_call_id.clone(),
            recipient_jid.clone(),
            recipient_name,
            is_video,
        );
        self.call_state.set_outgoing(call);

        client.start_call(&recipient_jid, is_video, placeholder_call_id);
        cx.notify();
    }

    /// Bring a minimised call back, or focus the card if it is already open.
    pub fn return_to_call(&mut self, cx: &mut Context<Self>) {
        if self.call_state.is_busy() {
            self.call_card.set_minimized(false);
            cx.notify();
        }
    }

    /// Leave a record of a call in the conversation it belonged to.
    ///
    /// The record is local: the daemon does not persist call history, so this
    /// survives the session and not a restart. Better than nothing — a missed
    /// call the user never saw is the case this exists for — and it is why the
    /// row is built from what the UI watched rather than queried back.
    pub(super) fn record_call(&mut self, stage: &Stage, cx: &mut Context<Self>) {
        self.record_call_as(stage, None, cx);
    }

    /// Take the daemon's call state as authoritative.
    ///
    /// With one wrinkle. The daemon's state update and the session's
    /// `CallEnded` news travel on two channels, and the state one is served
    /// first, so a call the peer ended can be gone from the state before the
    /// event that writes it down arrives — and the stage is where the
    /// duration and the direction live. It is recorded here, on the way out.
    /// Recording it twice is harmless: a record is keyed by the call id and
    /// `Chat::add_message` refuses a duplicate.
    pub(super) fn adopt_calls(&mut self, mut calls: CallState, cx: &mut Context<Self>) {
        // A caller parked behind the one on screen can hang up before ever
        // reaching it. The stage does not move when that happens, so nothing
        // here saw it and the conversation lost them entirely — no missed
        // call, no row, nothing. Promotion is not this: a promoted caller is
        // still held, as the stage.
        let abandoned = self
            .call_state
            .waiting()
            .filter(|waiting| !calls.holds(waiting.call_id()))
            .cloned();
        if let Some(waiting) = abandoned {
            match calls.ending_for(waiting.call_id()) {
                Some(Ending::Nothing) => {}
                ending => {
                    let outcome = match ending {
                        Some(Ending::As(outcome)) => Some(outcome),
                        _ => None,
                    };
                    self.record_call_as(&Stage::Incoming(waiting.into_call()), outcome, cx);
                }
            }
        }
        let ended = self
            .call_state
            .stage()
            .filter(|stage| !calls.still_holds(stage))
            .cloned();
        if let Some(stage) = ended {
            // Unless the daemon says there is nothing to write down: another
            // of this account's devices took it, or it was never placed at
            // all. A stage that merely disappears reads as missed when it was
            // incoming and as an attempt when it was outgoing, and a call
            // answered on the phone is the opposite of missed — the badge and
            // the "call back" prompt were for something already dealt with.
            match calls.ending_for(stage.call_id()) {
                Some(Ending::Nothing) => {}
                ending => {
                    let outcome = match ending {
                        Some(Ending::As(outcome)) => Some(outcome),
                        _ => None,
                    };
                    self.record_call_as(&stage, outcome, cx);
                }
            }
            // A card minimised for that call must not swallow the next ring.
            self.call_card.call_ended();
        }
        self.name_callers(&mut calls);
        let live = calls.active().is_some();
        self.call_state = calls;
        // The duration on the card is a clock, and a clock nobody winds shows
        // the second it started at. Armed here rather than off `CallAccepted`,
        // because a call this window did not answer — the daemon accepted it,
        // or another front end did — never produces that event here.
        if live {
            self.ensure_tick(cx);
        }
        cx.notify();
    }

    /// Hand the keyboard to whichever overlay should have it, and hand it
    /// back when none should.
    ///
    /// The card's Enter and Escape are scoped to its key context, and the
    /// viewer's arrow keys to its own, so they only fire while something
    /// focuses them — and nothing did, which made "enter accepts · esc
    /// declines" a promise the window did not keep and left the viewer's
    /// focus on a control that had stopped being rendered.
    ///
    /// A ringing call outranks the viewer: it is the more urgent of the two
    /// and the shorter-lived. An *answered* call owns nothing, because a call
    /// people talk through is a call they type through — which is also why
    /// mute is a window-wide chord rather than a card binding.
    ///
    /// Driven from the render pass because focusing needs a `Window` and the
    /// state it follows arrives from the daemon, which has none. It acts only
    /// on a change, so clicking into the composer while a phone rings does not
    /// start a fight for the caret.
    pub fn sync_overlay_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wanted = match self
            .call_state
            .stage()
            .filter(|stage| !matches!(stage, Stage::Active(_)))
        {
            Some(stage) => KeyboardOwner::RingingCall(stage.call_id().to_string()),
            None if self.media_viewer.is_some() => KeyboardOwner::Viewer,
            None if self.showing_settings() => KeyboardOwner::Screen,
            None => KeyboardOwner::Composer,
        };
        if wanted == self.keyboard_owner {
            return;
        }
        match &wanted {
            KeyboardOwner::RingingCall(_) => window.focus(&self.call_focus, cx),
            KeyboardOwner::Viewer => window.focus(&self.viewer_focus, cx),
            // Nothing to hand it to, and nothing that needs handing: see the
            // variant.
            KeyboardOwner::Screen => {}
            KeyboardOwner::Composer => self.focus_composer(window, cx),
        }
        self.keyboard_owner = wanted;
    }

    /// Put the names this window knows onto the calls the daemon sent.
    ///
    /// The daemon names a caller from its own chat list, which is the same
    /// list — but a chat renamed by a push name this window has already folded
    /// in would otherwise show the older name until the daemon reloads. Two
    /// lookups at most, and only while a call is up.
    fn name_callers(&self, calls: &mut CallState) {
        let named = |jid: &str| self.find_chat(jid).map(|chat| chat.name.clone());
        if let Some(call) = calls.incoming_mut()
            && let Some(name) = named(&call.caller_jid)
        {
            call.caller_name = name;
        }
        if let Some(call) = calls.waiting_mut()
            && let Some(name) = named(&call.caller_jid)
        {
            call.caller_name = name;
        }
    }

    /// Write a call down, with `outcome` when the caller knows better than the
    /// stage does — declining is the case: the stage still says "incoming",
    /// and only the person who pressed the button knows it was refused rather
    /// than missed.
    fn record_call_as(
        &mut self,
        stage: &Stage,
        outcome: Option<CallOutcome>,
        cx: &mut Context<Self>,
    ) {
        let (peer_jid, is_video, derived, is_outgoing) = match stage {
            Stage::Active(call) => (
                call.peer_jid.clone(),
                call.is_video,
                CallOutcome::Completed(call.elapsed().num_seconds().max(0) as u32),
                call.is_outgoing,
            ),
            // Never answered, from either side.
            Stage::Incoming(call) => (
                call.caller_jid.clone(),
                call.is_video,
                CallOutcome::Missed,
                false,
            ),
            Stage::Outgoing(call) => (
                call.recipient_jid.clone(),
                call.is_video,
                CallOutcome::Missed,
                true,
            ),
        };

        let record = CallRecord {
            is_video,
            is_outgoing,
            outcome: outcome.unwrap_or(derived),
        };
        let mut message = ChatMessage::new_incoming(
            format!("call-{}", stage.call_id()),
            peer_jid.clone(),
            String::new(),
        );
        // Written as an incoming row because that is what a system notice is,
        // but the user was there: they took the call, placed it, or refused it.
        // Only a call that rang unanswered is news, and only that one earns a
        // badge.
        message.is_read = !record.is_missed_inbound();
        message.system = Some(SystemNotice::Call(record));

        // A call can be the first thing that ever happens with a peer, and a
        // record with nowhere to go is a call the user is left with no trace
        // of. See `ensure_chat`.
        self.ensure_chat(&peer_jid);
        if self.add_message_to_chat(&peer_jid, message) {
            self.invalidate_message_cache(&peer_jid);
            self.invalidate_chat_cache();
            cx.notify();
        }
    }

    /// Keep a one-second repaint alive while a call is up.
    ///
    /// The duration is derived from a timestamp rather than counted, so
    /// nothing drifts if a tick is late or missed — the tick only asks for a
    /// repaint. It stops as soon as the call ends, so an idle client is not
    /// waking once a second forever.
    pub(super) fn ensure_tick(&mut self, cx: &mut Context<Self>) {
        if self.tick_task.is_some() {
            return;
        }
        self.tick_task = Some(cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            loop {
                smol::Timer::after(std::time::Duration::from_secs(1)).await;
                let keep_going = entity.update(cx, |app, cx| {
                    // Expiring a stale typing notice matters even with no
                    // call up: the peer that stopped may never say so.
                    let presence_changed = app.presence.prune();
                    if presence_changed {
                        app.invalidate_chat_cache();
                    }
                    if app.call_state.active().is_some() || presence_changed {
                        cx.notify();
                    }
                    app.call_state.active().is_some() || app.presence.has_typing()
                });
                match keep_going {
                    Ok(true) => continue,
                    // Either nothing left to tick, or the view is gone.
                    Ok(false) | Err(_) => break,
                }
            }
            let _ = entity.update(cx, |app, _| app.tick_task = None);
        }));
    }
}
