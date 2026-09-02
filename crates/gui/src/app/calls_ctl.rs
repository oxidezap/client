//! Call controls, one level above `client.voip()`.

use std::sync::Arc;

use gpui::{Pixels, Point, RenderImage};
use oxidezap_core::{IncomingCall, VideoStream};

use super::*;
use crate::video::CallFrame;

/// The newest picture of each direction of the call on screen.
///
/// One frame per direction and nothing behind it. A queue here would be
/// latency between the person talking and the person watching, and the frame
/// that is late is exactly the one worth dropping.
///
/// Keyed by call, because a picture belongs to the call it was taken in: a
/// frame that arrives after the call it came from has ended — the socket is
/// one hop behind the state — would otherwise be drawn into the next one.
#[derive(Default)]
pub struct CallPictures {
    call_id: Option<String>,
    local: Option<Arc<RenderImage>>,
    remote: Option<Arc<RenderImage>>,
}

impl CallPictures {
    fn accept(&mut self, frame: CallFrame) {
        if self.call_id.as_deref() != Some(frame.call_id.as_str()) {
            *self = Self {
                call_id: Some(frame.call_id),
                ..Self::default()
            };
        }
        match frame.stream {
            VideoStream::Local => self.local = Some(frame.image),
            VideoStream::Remote => self.remote = Some(frame.image),
        }
    }

    /// Drop what no longer has a camera behind it.
    ///
    /// Driven off the call state rather than off the frames stopping: a
    /// camera that is switched off simply stops sending, and a pane left
    /// holding its last frame is a photograph of somebody who has gone.
    fn follow(&mut self, calls: &CallState) {
        let Some(call) = calls.active() else {
            *self = Self::default();
            return;
        };
        if self.call_id.as_deref() != Some(call.call_id.as_str()) {
            *self = Self::default();
            return;
        }
        if !call.video.local {
            self.local = None;
        }
        if !call.video.remote {
            self.remote = None;
        }
    }

    pub fn of(&self, stream: VideoStream) -> Option<&Arc<RenderImage>> {
        match stream {
            VideoStream::Local => self.local.as_ref(),
            VideoStream::Remote => self.remote.as_ref(),
        }
    }
}

/// A call on its way into the conversation: the stage it ended from, and the
/// outcome where the gesture knows better than the stage does.
///
/// Handed back rather than written down here. How a call ended is said in the
/// state, and this is that answer travelling — but *where* it gets written is
/// a chat, and a chat is the window's.
pub(super) struct Ended {
    stage: Stage,
    outcome: Option<CallOutcome>,
}

/// What call is happening, where this window draws it, and what it has asked
/// for that has not come back yet.
///
/// An entity rather than six fields on the app, and the type of the context
/// every method here takes is what makes that more than tidying: none of them
/// can reach a `Context<WhatsAppApp>`, so none of them can mark the chats, the
/// drafts or the selection as having moved because a frame of video arrived.
///
/// What stays above it is everything that needs the session or a conversation:
/// commanding the daemon, and writing a call down. That line is why the
/// methods here hand back an [`Ended`] and an ask instead of acting on them —
/// a record is a message in a chat, and this holds no chats.
pub(super) struct Calls {
    /// What call is happening. Adopted whole from the daemon on attach, and
    /// advanced by the same events the daemon applies to its own copy.
    state: CallState,
    /// Where *this* window puts the card for it. The card belongs to the
    /// window, not to the conversation.
    card: CallCard,
    /// The newest decoded picture of each of the call's two directions.
    ///
    /// One frame per direction and no history: this is a stream, and a
    /// backlog of pictures is latency between the person talking and the
    /// person watching. Cleared with the call, because the last frame of a
    /// call that has ended is not something to keep drawing.
    pictures: CallPictures,
    /// What this window last asked the camera to do, until the daemon agrees.
    ///
    /// Opening a camera is device work and, the first time, a permission
    /// prompt — seconds during which the state still says the camera is off.
    /// A toggle computed from that state alone asks to turn it *on* again on
    /// every click, so somebody who changed their mind could not say so until
    /// the camera they no longer wanted had finished coming on.
    video_asked: Option<(String, bool)>,
    /// The same, for the microphone.
    ///
    /// The announcement is a round trip through the daemon and the peer, and
    /// every other call frame in between — the peer turning a camera on, a
    /// waiting call promoted — carries the mute the daemon still holds. That
    /// took the button back to "open", and the next press computed its toggle
    /// from that stale value and asked to unmute a microphone the user
    /// believed was muted.
    muted_asked: Option<(String, bool)>,
    /// Repaints the call duration, and expires stale typing notices. Only
    /// alive while there is something to tick.
    tick: Option<Task<()>>,
}

impl Calls {
    pub(super) fn new() -> Self {
        Self {
            state: CallState::new(),
            card: CallCard::default(),
            pictures: CallPictures::default(),
            video_asked: None,
            muted_asked: None,
            tick: None,
        }
    }

    pub(super) fn state(&self) -> &CallState {
        &self.state
    }

    /// This window's placement of the card, which no other window shares.
    pub(super) fn card(&self) -> &CallCard {
        &self.card
    }

    /// The newest picture of one direction of the live call, when there is
    /// one to draw.
    pub(super) fn picture(&self, stream: VideoStream) -> Option<&Arc<RenderImage>> {
        self.pictures.of(stream)
    }

    /// Whether this window's camera is on, or on its way there.
    ///
    /// What was asked for outranks what the state says while the ask is still
    /// outstanding: a camera takes seconds to open, and a control that stayed
    /// "off" for all of them reads as a click that did nothing.
    pub(super) fn video_showing(&self) -> bool {
        match &self.video_asked {
            Some((call_id, wanted)) if self.state.holds(call_id) => *wanted,
            _ => self.state.video().local,
        }
    }

    /// Take the ringing offer, for a window that is about to answer it.
    pub(super) fn take_incoming(&mut self) -> Option<IncomingCall> {
        self.state.take_incoming()
    }

    /// The offer this window answered is now the live call.
    pub(super) fn accepted(
        &mut self,
        call: &IncomingCall,
        app: WeakEntity<WhatsAppApp>,
        cx: &mut Context<Self>,
    ) {
        self.state.connect_accepted(call);
        self.ensure_tick(app, cx);
        cx.notify();
    }

    /// The call this window placed, drawn before the daemon has confirmed it.
    pub(super) fn place_outgoing(&mut self, call: OutgoingCall, cx: &mut Context<Self>) {
        self.state.set_outgoing(call);
        cx.notify();
    }

    /// What to ask the camera for, which is the opposite of what it is doing.
    ///
    /// Asked for rather than applied: the daemon owns the device, and what
    /// comes back is what it managed to do. A camera that will not open would
    /// otherwise leave the button showing a picture nobody is being sent —
    /// the same reason mute is asked for rather than computed here.
    pub(super) fn ask_video(&mut self, cx: &mut Context<Self>) -> Option<(String, bool)> {
        let call = self.state.active()?;
        // Toggled against what was last *asked* for, where that is still
        // outstanding: the state cannot have caught up with a camera that is
        // still opening, and a second click means the opposite of the first
        // rather than the same thing again.
        let call_id = call.call_id.clone();
        let showing = match &self.video_asked {
            Some((asked_for, wanted)) if *asked_for == call_id => *wanted,
            _ => call.video.local,
        };
        let wanted = !showing;
        self.video_asked = Some((call_id.clone(), wanted));
        cx.notify();
        Some((call_id, wanted))
    }

    /// The same for the microphone, which the state can toggle itself.
    pub(super) fn ask_muted(&mut self, cx: &mut Context<Self>) -> Option<(String, bool)> {
        let call_id = self.state.active().map(|c| c.call_id.clone())?;
        let muted = self.state.toggle_muted()?;
        // Held until the daemon answers, the way the camera's ask is: what
        // comes back from the device is the last word, and until it does, no
        // unrelated call frame may take this back.
        self.muted_asked = Some((call_id.clone(), muted));
        cx.notify();
        Some((call_id, muted))
    }

    /// What the daemon's microphone really did, as the answer to what was
    /// asked for here.
    ///
    /// The announcement is what ends the ask, not the state frames arriving
    /// in the meantime — those carry the mute the daemon still held. It is
    /// applied whether or not it agrees with what was asked, which is what
    /// makes it the last word: an unmute the peer was never told about leaves
    /// the device muted, and this is what draws that rather than what was
    /// wanted.
    pub(super) fn settle_muted(&mut self, call_id: &str, muted: bool, cx: &mut Context<Self>) {
        if !self.state.holds(call_id) {
            return;
        }
        self.state.set_muted(&call_id.to_string(), muted);
        if self
            .muted_asked
            .as_ref()
            .is_some_and(|(asked_for, _)| asked_for == call_id)
        {
            self.muted_asked = None;
        }
        cx.notify();
    }

    /// What the daemon's camera really did, as the answer to what was asked
    /// for here.
    ///
    /// Folded into the call state the same way the daemon folds it into its
    /// own, because the two channels are independent and this one can arrive
    /// first: waiting for the state frame would draw the old value for a
    /// moment, and where the settle agrees with what the daemon already held
    /// there is no state frame at all.
    pub(super) fn settle_video(
        &mut self,
        call_id: &String,
        stream: VideoStream,
        on: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.state.holds(call_id) {
            return;
        }
        self.state.set_video(call_id, stream, on);
        self.pictures.follow(&self.state);
        // Only this side's camera is anything anyone here asked for.
        if stream == VideoStream::Local
            && self
                .video_asked
                .as_ref()
                .is_some_and(|(asked_for, _)| asked_for == call_id)
        {
            self.video_asked = None;
        }
        cx.notify();
    }

    /// One decoded frame, straight onto the pane that draws it.
    pub(super) fn draw_frame(&mut self, frame: CallFrame, cx: &mut Context<Self>) {
        // Frames and state travel on different channels, so one can arrive
        // after the state that ended what it belongs to. Both halves of that
        // are checked: the call, because drawing it into the next one would
        // put the last person's face on this one; and the direction, because
        // `follow` has already cleared the pane for a camera that went off
        // and a frame still in flight would light it again — for good, since
        // no later state change would come to clear it a second time.
        if !self.state.holds(&frame.call_id) || !self.state.video().is_on(frame.stream) {
            return;
        }
        self.pictures.accept(frame);
        cx.notify();
    }

    /// Refuse the caller parked behind the one on screen, and hand back what
    /// to write down for them.
    pub(super) fn refuse_waiting(&mut self, cx: &mut Context<Self>) -> Option<(String, Ended)> {
        let waiting = self.state.take_waiting()?;
        let call_id = waiting.call_id().to_string();
        cx.notify();
        Some((
            call_id,
            // Written down like any other refusal. Left out, a caller the
            // user saw and refused left no trace anywhere: the strip is gone,
            // the daemon only sends the network decline, and the conversation
            // has no row saying they rang.
            Ended {
                stage: Stage::Incoming(waiting.into_call()),
                outcome: Some(CallOutcome::Declined),
            },
        ))
    }

    /// Refuse the offer the card is showing.
    ///
    /// `decline_incoming`, not `take_incoming`: refusing an offer replaces it
    /// with nothing, so a caller parked behind it has to come forward. Taking
    /// the stage without promoting left the optimistic state with no stage at
    /// all, and the card drew neither caller until a daemon update repaired
    /// it — or, if the decline never landed, not at all.
    pub(super) fn refuse_incoming(&mut self, cx: &mut Context<Self>) -> Option<IncomingCall> {
        let call = self.state.decline_incoming()?;
        cx.notify();
        Some(call)
    }

    /// End whatever call is up, and say what it was.
    pub(super) fn end(&mut self, cx: &mut Context<Self>) -> Option<(Stage, Ended)> {
        let stage = self.state.take()?;
        self.card.call_ended();
        self.pictures = CallPictures::default();
        self.video_asked = None;
        // A ringing offer ended by this button was refused, not missed — the
        // same thing a decline writes down, and the phone viewport routes its
        // Decline through here. Deriving the outcome from the stage alone
        // gave the two buttons two different histories for one gesture.
        let outcome = matches!(stage, Stage::Incoming(_)).then_some(CallOutcome::Declined);
        cx.notify();
        Some((stage.clone(), Ended { stage, outcome }))
    }

    pub(super) fn is_busy(&self) -> bool {
        self.state.is_busy()
    }

    pub(super) fn set_minimized(&mut self, minimized: bool, cx: &mut Context<Self>) {
        self.card.set_minimized(minimized);
        cx.notify();
    }

    /// Bring a minimised call back, or leave an open card alone.
    pub(super) fn unminimize(&mut self, cx: &mut Context<Self>) {
        if self.state.is_busy() {
            self.card.set_minimized(false);
            cx.notify();
        }
    }

    pub(super) fn begin_drag(&mut self, at: Point<Pixels>) {
        self.card.begin_drag(at);
    }

    /// The pointer moved while dragging the card.
    ///
    /// The bounds come from the window and the card's own measured size, so
    /// they follow a resize, a density change, and the card changing shape
    /// mid-call without anything here knowing how big it is.
    pub(super) fn drag_to(
        &mut self,
        at: Point<Pixels>,
        viewport: gpui::Size<Pixels>,
        inset: Pixels,
        cx: &mut Context<Self>,
    ) {
        if self.card.drag_to(at) {
            self.card.clamp_to(viewport, inset);
            cx.notify();
        }
    }

    pub(super) fn end_drag(&mut self) {
        self.card.end_drag();
    }

    /// Take the daemon's call state as authoritative, and hand back the calls
    /// that ended on the way.
    ///
    /// The daemon's state update and the session's `CallEnded` news travel on
    /// two channels, and the state one is served first, so a call the peer
    /// ended can be gone from the state before the event that writes it down
    /// arrives — and the stage is where the duration and the direction live.
    /// It is reported here, on the way out. Reporting it twice is harmless: a
    /// record is keyed by the call id and `Chat::add_message` refuses a
    /// duplicate.
    ///
    /// `calls` arrives already named by the window, which is the one thing
    /// about a call this cannot answer for itself: a caller's name comes from
    /// the chat list.
    pub(super) fn adopt(
        &mut self,
        calls: CallState,
        app: WeakEntity<WhatsAppApp>,
        cx: &mut Context<Self>,
    ) -> Vec<Ended> {
        let mut ended = Vec::new();
        // A caller parked behind the one on screen can hang up before ever
        // reaching it. The stage does not move when that happens, so nothing
        // here saw it and the conversation lost them entirely — no missed
        // call, no row, nothing. Promotion is not this: a promoted caller is
        // still held, as the stage.
        let abandoned = self
            .state
            .waiting()
            .filter(|waiting| !calls.holds(waiting.call_id()))
            .cloned();
        if let Some(waiting) = abandoned
            && let Some(outcome) = Self::ending(&calls, waiting.call_id())
        {
            ended.push(Ended {
                stage: Stage::Incoming(waiting.into_call()),
                outcome,
            });
        }
        let gone = self
            .state
            .stage()
            .filter(|stage| !calls.still_holds(stage))
            .cloned();
        if let Some(stage) = gone {
            if let Some(outcome) = Self::ending(&calls, stage.call_id()) {
                ended.push(Ended { stage, outcome });
            }
            // A card minimised for that call must not swallow the next ring.
            self.card.call_ended();
        }
        let live = calls.active().is_some();
        // A mute this window asked for and the daemon has not answered yet
        // survives the frame. Every other call frame carries the mute the
        // daemon still holds, and letting one of those land put the button
        // back to "open" over a microphone on its way to muted — with the
        // next press computing its toggle from that.
        let pending_mute = self
            .muted_asked
            .take()
            .filter(|(call_id, _)| calls.holds(call_id));
        self.state = calls;
        if let Some((call_id, wanted)) = &pending_mute {
            self.state.set_muted(call_id, *wanted);
        }
        self.muted_asked = pending_mute;
        // After the state, because what a picture may still be drawn for is
        // exactly what the new state says has a camera behind it.
        self.pictures.follow(&self.state);
        // A request the daemon has answered is not outstanding any more, and
        // one whose call is gone answers itself.
        if self.video_asked.as_ref().is_none_or(|(call_id, wanted)| {
            !self.state.holds(call_id) || self.state.video().local == *wanted
        }) {
            self.video_asked = None;
        }
        // The duration on the card is a clock, and a clock nobody winds shows
        // the second it started at. Armed here rather than off `CallAccepted`,
        // because a call this window did not answer — the daemon accepted it,
        // or another front end did — never produces that event here.
        if live {
            self.ensure_tick(app, cx);
        }
        cx.notify();
        ended
    }

    /// Whether a call the daemon has stopped holding is worth writing down,
    /// and as what.
    ///
    /// `None` is the daemon saying there is nothing to write: another of this
    /// account's devices took it, or it was never placed at all. A stage that
    /// merely disappears reads as missed when it was incoming and as an
    /// attempt when it was outgoing, and a call answered on the phone is the
    /// opposite of missed — the badge and the "call back" prompt were for
    /// something already dealt with.
    ///
    /// The nesting is the reason this has a name rather than being written
    /// out at both callers: `Some(None)` is "write it down, and let the stage
    /// say how", which is not the same answer as `None`.
    fn ending(calls: &CallState, call_id: &str) -> Option<Option<CallOutcome>> {
        match calls.ending_for(call_id) {
            Some(Ending::Nothing) => None,
            Some(Ending::As(outcome)) => Some(Some(outcome)),
            None => Some(None),
        }
    }

    /// Everything about a call this account was having, dropped.
    ///
    /// A call is account state as much as a chat is. Left standing, the next
    /// daemon's first (empty) snapshot reads as this stage ending, and the
    /// record is written into the account that has just been paired —
    /// recreating a chat for the old account's peer to hold it.
    pub(super) fn forget(&mut self, cx: &mut Context<Self>) {
        self.state = CallState::new();
        self.card.call_ended();
        // Including the pictures: a frame of the old account's peer left in a
        // pane is exactly the kind of thing a reset exists to remove.
        self.pictures = CallPictures::default();
        self.video_asked = None;
        self.muted_asked = None;
        cx.notify();
    }

    /// Keep a one-second repaint alive while a call is up.
    ///
    /// The duration is derived from a timestamp rather than counted, so
    /// nothing drifts if a tick is late or missed — the tick only asks for a
    /// repaint. It stops as soon as the call ends, so an idle client is not
    /// waking once a second forever.
    ///
    /// The repaint it asks for is the *window's*, and deliberately so: the
    /// card belongs to the window and is drawn from the root's render pass,
    /// so repainting this entity alone would leave the duration frozen at the
    /// second the call started. The window is also where the other thing on
    /// this clock lives — a typing notice whose peer never said it stopped —
    /// which is why the loop outlasts the call whenever somebody is typing.
    pub(super) fn ensure_tick(&mut self, app: WeakEntity<WhatsAppApp>, cx: &mut Context<Self>) {
        if self.tick.is_some() {
            return;
        }
        self.tick = Some(cx.spawn(async move |me: WeakEntity<Self>, cx| {
            loop {
                crate::platform::sleep(std::time::Duration::from_secs(1)).await;
                let Ok(live) = me.update(cx, |calls, _| calls.state.active().is_some()) else {
                    break;
                };
                let keep_going = app.update(cx, |app, cx| {
                    // Expiring a stale typing notice matters even with no
                    // call up: the peer that stopped may never say so.
                    let presence_changed = app.presence.prune();
                    if presence_changed {
                        app.invalidate_chat_cache();
                    }
                    if live || presence_changed {
                        cx.notify();
                    }
                    live || app.presence.has_typing()
                });
                match keep_going {
                    Ok(true) => continue,
                    // Either nothing left to tick, or the window is gone.
                    Ok(false) | Err(_) => break,
                }
            }
            let _ = me.update(cx, |calls, _| calls.tick = None);
        }));
    }
}

impl WhatsAppApp {
    pub fn call_state<'a>(&self, cx: &'a App) -> &'a CallState {
        self.calls.read(cx).state()
    }

    /// This window's placement of the card, which no other window shares.
    pub fn call_card<'a>(&self, cx: &'a App) -> &'a CallCard {
        self.calls.read(cx).card()
    }

    pub fn active_call<'a>(&self, cx: &'a App) -> Option<&'a ActiveCall> {
        self.calls.read(cx).state().active()
    }

    /// The newest picture of one direction of the live call, when there is
    /// one to draw.
    pub fn call_picture<'a>(
        &self,
        stream: VideoStream,
        cx: &'a App,
    ) -> Option<&'a Arc<RenderImage>> {
        self.calls.read(cx).picture(stream)
    }

    /// Whether this window's camera is on, or on its way there.
    pub fn call_video_showing(&self, cx: &App) -> bool {
        self.calls.read(cx).video_showing()
    }

    /// Whether the peer is waiting on this side to turn its camera on.
    ///
    /// Read from the call state rather than remembered here: the request is
    /// something the daemon holds, so a window that attaches mid-call is
    /// handed it like everything else about the call.
    pub fn call_video_requested(&self, cx: &App) -> bool {
        self.calls.read(cx).state().video().requested
    }

    /// Accept the incoming call.
    ///
    /// The media comes up here, so the call becomes active immediately rather
    /// than waiting for an answer event that never arrives for an inbound
    /// call. That gap is what used to leave the audio running with no UI.
    pub fn accept_call(&mut self, cx: &mut Context<Self>) {
        if self.client.is_none() {
            warn!("Cannot accept call: client is unavailable");
            return;
        }
        // The same question the outgoing path asks, and it has to be asked
        // here too: an offer arrives whatever this browser can carry, so a
        // page that cannot hold the media would otherwise open the
        // microphone, accept, and end the call at relay setup. Declined
        // rather than ignored — the caller is ringing, and the honest answer
        // is no rather than silence until their own timeout.
        // Before the refusal below, and deliberately *not* folded into it:
        // this window could carry a call perfectly well, it is simply the
        // wrong one. Declining here would send `Decline` to the leader and
        // clear the offer everywhere — telling somebody to answer in the
        // other tab while destroying the call they would have answered. So
        // the offer is left ringing, in this tab and in that one.
        if let Some(reason) = crate::platform::calls_belong_to_another_tab() {
            warn!("Not accepting here: {reason}");
            self.notify_user(reason, crate::app::notices::Tone::Problem, cx);
            return;
        }
        if let Some(reason) = crate::platform::calls_unavailable() {
            warn!("Cannot accept call: {reason}");
            self.notify_user(reason, crate::app::notices::Tone::Problem, cx);
            self.decline_call(cx);
            return;
        }

        let Some(call) = self.calls.update(cx, |calls, _| calls.take_incoming()) else {
            return;
        };
        // A video offer this window cannot decode is still a call worth
        // taking: the audio works, and the only thing missing is the picture.
        //
        // Said rather than acted on, because the two obvious actions are both
        // worse and one of them is not ours to take. Declining throws away a
        // conversation over a pane. Answering it as voice is the daemon's
        // decision and deliberately not a front end's — it reads `is_video`
        // off the ringing offer rather than taking our word, since the
        // library refuses `.video()` on an audio offer — so there is no way
        // from here to accept a video call as anything else. What is left,
        // and what the person actually needs, is knowing why the picture
        // never arrives instead of watching two panes wait forever.
        if call.is_video
            && let Some(reason) = crate::platform::video_decode_unavailable()
        {
            warn!("Accepting a video call this window cannot draw: {reason}");
            self.notify_user(
                "This browser cannot show video, so you will hear this call but not see it.",
                crate::app::notices::Tone::Problem,
                cx,
            );
        }
        info!(
            "Accepting call {} from {}",
            call.call_id,
            observe_str(&call.caller_jid)
        );
        let Some(client) = &self.client else {
            // Checked at the top; re-read here because the notice above needs
            // `self`, and a borrow held across it would outlive it.
            warn!("Cannot accept call: client is unavailable");
            return;
        };
        client.accept_call(call.call_id.as_str());
        let app = cx.entity().downgrade();
        self.calls
            .update(cx, |calls, cx| calls.accepted(&call, app, cx));
    }

    /// Turn this window's camera on or off.
    ///
    /// What is asked for is the entity's; asking the daemon is the window's,
    /// because the session is.
    pub fn toggle_call_video(&mut self, cx: &mut Context<Self>) {
        let Some((call_id, wanted)) = self.calls.update(cx, |calls, cx| calls.ask_video(cx)) else {
            return;
        };
        if let Some(client) = &self.client {
            client.set_call_video(&call_id, wanted);
        }
    }

    /// Mute or unmute the live call.
    pub fn toggle_call_muted(&mut self, cx: &mut Context<Self>) {
        let Some((call_id, muted)) = self.calls.update(cx, |calls, cx| calls.ask_muted(cx)) else {
            return;
        };
        if let Some(client) = &self.client {
            client.set_call_muted(&call_id, muted);
        }
    }

    pub(super) fn settle_call_muted(&mut self, call_id: &str, muted: bool, cx: &mut Context<Self>) {
        self.calls
            .update(cx, |calls, cx| calls.settle_muted(call_id, muted, cx));
    }

    pub(super) fn settle_call_video(
        &mut self,
        call_id: &String,
        stream: VideoStream,
        on: bool,
        cx: &mut Context<Self>,
    ) {
        self.calls
            .update(cx, |calls, cx| calls.settle_video(call_id, stream, on, cx));
    }

    /// Draw whatever pictures were waiting when the window got to them.
    ///
    /// Taken from the slot rather than delivered: what arrived while the
    /// window was busy is not a backlog to work through but one picture per
    /// direction, the newest, and the ones it replaced were never worth
    /// drawing.
    pub(super) fn draw_waiting_call_frames(&mut self, cx: &mut Context<Self>) {
        let Some(waiting) = self.client.as_ref().map(|c| c.call_frames().take()) else {
            return;
        };
        self.calls.update(cx, |calls, cx| {
            for frame in waiting {
                calls.draw_frame(frame, cx);
            }
        });
    }

    /// Refuse the second call parked behind the one on screen.
    ///
    /// Its own command, reached from its own strip on the card. Folding it
    /// into `decline_call` made the *visible* Decline button refuse a caller
    /// the user could not see, and leave the ringing one ringing.
    pub fn decline_waiting_call(&mut self, cx: &mut Context<Self>) {
        // Before the call is taken out of the state: with no daemon to reach,
        // nothing refuses anybody. The caller goes on ringing, and writing the
        // refusal down anyway put a line in the conversation saying the call
        // was declined when nothing had been declined at all. The visible
        // Decline says the same by returning early.
        if self.client.is_none() {
            warn!("Cannot decline the waiting call: client is unavailable");
            return;
        }
        let Some((call_id, ended)) = self.calls.update(cx, |calls, cx| calls.refuse_waiting(cx))
        else {
            return;
        };
        info!("Declining waiting call {call_id}");
        if let Some(client) = &self.client {
            client.decline_call(call_id.as_str());
        }
        self.record_call(ended, cx);
    }

    /// Decline the incoming call the card is showing.
    pub fn decline_call(&mut self, cx: &mut Context<Self>) {
        if self.client.is_none() {
            warn!("Cannot decline call: client is unavailable");
            return;
        }
        let Some(call) = self.calls.update(cx, |calls, cx| calls.refuse_incoming(cx)) else {
            return;
        };
        info!(
            "Declining call {} from {}",
            call.call_id,
            observe_str(&call.caller_jid)
        );
        if let Some(client) = &self.client {
            client.decline_call(call.call_id.as_str());
        }
        // A refusal is not a missed call, and a local reject emits no
        // `CallEnded` to write one later — so the record is made here, saying
        // what actually happened. The mobile decline goes through `hang_up`,
        // which records it as missed; this is the outcome the enum has been
        // carrying unused.
        self.record_call(
            Ended {
                stage: Stage::Incoming(call),
                outcome: Some(CallOutcome::Declined),
            },
            cx,
        );
    }

    /// End whatever call is up: cancel a call we placed, decline one ringing
    /// at us, or hang up a live one.
    ///
    /// One method because it is one gesture. The card decides what to *call*
    /// it — cancelling an unanswered call is not hanging up on someone — but
    /// the effect is the same and splitting it invites the two to drift.
    pub fn hang_up(&mut self, cx: &mut Context<Self>) {
        let Some((stage, ended)) = self.calls.update(cx, |calls, cx| calls.end(cx)) else {
            return;
        };
        let call_id = stage.call_id().to_string();
        info!("Ending call {call_id}");
        self.record_call(ended, cx);
        if let Some(client) = &self.client {
            match &stage {
                // A ringing offer has to be rejected rather than hung up:
                // there is no live handle, and the caller should stop ringing
                // instead of waiting out the timeout.
                Stage::Incoming(_) => client.decline_call(&call_id),
                Stage::Outgoing(_) | Stage::Active(_) => client.cancel_call(&call_id),
            }
        }
    }

    pub fn set_call_minimized(&mut self, minimized: bool, cx: &mut Context<Self>) {
        self.calls
            .update(cx, |calls, cx| calls.set_minimized(minimized, cx));
    }

    pub fn begin_call_drag(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        self.calls.update(cx, |calls, _| calls.begin_drag(at));
    }

    /// The pointer moved while dragging the card.
    pub fn drag_call_card(
        &mut self,
        at: Point<Pixels>,
        viewport: gpui::Size<Pixels>,
        inset: Pixels,
        cx: &mut Context<Self>,
    ) {
        self.calls
            .update(cx, |calls, cx| calls.drag_to(at, viewport, inset, cx));
    }

    pub fn end_call_drag(&mut self, cx: &mut Context<Self>) {
        self.calls.update(cx, |calls, _| calls.end_drag());
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
        if self.client.is_none() {
            warn!("Cannot start call: client is unavailable");
            return;
        }

        // One call at a time: placing a second would leave the first with no
        // UI to end it.
        if self.calls.read(cx).is_busy() {
            warn!("A call is already in progress");
            return;
        }

        // Asked before the call is drawn rather than after it fails: a browser
        // with no WebRTC cannot carry the media whatever the account does, and
        // the alternative is somebody granting the microphone to a call that
        // was never going to connect. Said out loud, because unlike the
        // microphone there is no control to draw disabled -- the call button
        // is worth keeping for the desktop this same view runs on.
        // Nothing is ringing here, so this is the same refusal as the one
        // below rather than a different kind of answer — but it is a
        // different question, and keeping them apart is what stops the
        // accept path from declining a call it should have left alone.
        if let Some(reason) = crate::platform::calls_belong_to_another_tab() {
            warn!("Cannot start call here: {reason}");
            self.notify_user(reason, crate::app::notices::Tone::Problem, cx);
            return;
        }
        if let Some(reason) = crate::platform::calls_unavailable() {
            warn!("Cannot start call: {reason}");
            self.notify_user(reason, crate::app::notices::Tone::Problem, cx);
            return;
        }

        // The decoder is always this front end's, whoever holds the session:
        // a daemon can open its camera, negotiate video and send perfectly
        // while this window rejects every access unit, leaving both panes
        // waiting on a picture that is arriving. So it is asked separately
        // from `calls_unavailable`, which is about carrying the media.
        //
        // Downgraded rather than refused, which is what this module does with
        // every other camera that will not work — the call is worth placing,
        // and it is the picture that is not on offer.
        let is_video = if is_video {
            match crate::platform::video_decode_unavailable() {
                Some(reason) => {
                    warn!("Placing this call as voice: {reason}");
                    self.notify_user(
                        "This browser cannot show video, so this is a voice call.",
                        crate::app::notices::Tone::Problem,
                        cx,
                    );
                    false
                }
                None => true,
            }
        } else {
            false
        };

        let recipient_name = self
            .find_chat(&recipient_jid)
            .map(|chat| chat.name.clone())
            .unwrap_or_else(|| "Unknown contact".to_string());

        info!(
            "Starting {} call to {}",
            if is_video { "video" } else { "audio" },
            observe_str(&recipient_jid)
        );

        // Named the way an optimistic bubble is, and for the same reason: a
        // clock reading alone is one id per millisecond for the whole
        // machine, and two tabs are one process. Both would draw the same
        // placeholder, the daemon would accept one call and refuse the other
        // as busy — against an id it had already given the live one — and the
        // refusal would erase the surviving call's record while its own
        // rename became impossible to tell from the rejected attempt's.
        let placeholder_call_id = Self::next_local_id("ui-call");
        let call = OutgoingCall::new(
            placeholder_call_id.clone(),
            recipient_jid.clone(),
            recipient_name,
            is_video,
        );
        self.calls
            .update(cx, |calls, cx| calls.place_outgoing(call, cx));

        let Some(client) = &self.client else {
            // Checked at the top; re-read here because the checks between
            // need `self` and a borrow held across them would outlive them.
            warn!("Cannot start call: client is unavailable");
            return;
        };
        client.start_call(&recipient_jid, is_video, placeholder_call_id);
    }

    /// Keep the one-second clock alive for something that is not a call.
    ///
    /// A typing notice expires on the same clock the call duration is drawn
    /// on, and a peer that stops typing may never say so — so a `composing`
    /// with no `paused` behind it is a reason to wind it even with no call up.
    pub(super) fn ensure_tick(&mut self, cx: &mut Context<Self>) {
        let app = cx.entity().downgrade();
        self.calls
            .update(cx, |calls, cx| calls.ensure_tick(app, cx));
    }

    /// Bring a minimised call back, or focus the card if it is already open.
    pub fn return_to_call(&mut self, cx: &mut Context<Self>) {
        self.calls.update(cx, |calls, cx| calls.unminimize(cx));
    }

    /// Take the daemon's call state as authoritative, and write down whatever
    /// ended on the way.
    pub(super) fn adopt_calls(&mut self, mut calls: CallState, cx: &mut Context<Self>) {
        // Named here rather than inside the entity, because a caller's name
        // comes from this window's chat list.
        self.name_callers(&mut calls);
        let app = cx.entity().downgrade();
        let ended = self
            .calls
            .update(cx, |state, cx| state.adopt(calls, app, cx));
        for call in ended {
            self.record_call(call, cx);
        }
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
    ///
    /// Every frame, and not only the connected ones: this is the window's
    /// only route to having a keyboard at all. The tail of the list is what
    /// makes that true — a surface is named only while the frame is drawing
    /// it, and the window itself is what remains when none of them is, so
    /// there is no state in which the answer is "nobody". There used to be,
    /// and it was the state every launch started in: the composer was
    /// recorded as the owner before one existed, the first sync found nothing
    /// to change, and every window-level shortcut stayed dead until a click
    /// gave the window a focus of its own.
    pub fn sync_overlay_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ringing = self
            .calls
            .read(cx)
            .state()
            .stage()
            .filter(|stage| !matches!(stage, Stage::Active(_)))
            .map(|stage| stage.call_id().to_string());
        let wanted = keyboard_owner_for(
            ringing,
            self.keyboard_surfaces,
            self.keyboard_intent,
            self.showing_settings(cx),
        );
        if self.keyboard_owner.as_ref() == Some(&wanted) {
            return;
        }
        log::debug!("keyboard: {:?} -> {wanted:?}", self.keyboard_owner);
        match &wanted {
            KeyboardOwner::RingingCall(_) => window.focus(&self.call_focus, cx),
            KeyboardOwner::Viewer => {
                let handle = self.viewer.read(cx).focus().clone();
                window.focus(&handle, cx)
            }
            KeyboardOwner::ChatList => window.focus(&self.chat_list_focus, cx),
            // Escape is the way out of Settings and Escape is the window's,
            // not the screen's: see the variant.
            KeyboardOwner::Screen | KeyboardOwner::Root => window.focus(&self.root_focus, cx),
            KeyboardOwner::Composer => self.focus_composer(window, cx),
        }
        self.keyboard_owner = Some(wanted);
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

    /// Write a call down.
    ///
    /// The outcome [`Ended`] carries is used where the gesture knew better
    /// than the stage does — declining is the case: the stage still says
    /// "incoming", and only the person who pressed the button knows it was
    /// refused rather than missed.
    fn record_call(&mut self, ended: Ended, cx: &mut Context<Self>) {
        let Ended { stage, outcome } = ended;
        let (peer_jid, is_video, derived, is_outgoing) = match &stage {
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
        if self.add_message_to_chat(&peer_jid, message, cx) {
            self.invalidate_message_cache(&peer_jid, cx);
            self.invalidate_chat_cache();
            cx.notify();
        }
    }
}

/// Who should have the keyboard, given a ringing call and the surfaces this
/// frame drew.
///
/// Every arm asks what the frame *drew*, never what the state holds. The two
/// are not the same across a dropped connection: the viewer and the call both
/// survive it — `leave_connected_view` closes neither — while the error
/// screen that replaces the conversation draws neither of them. Handing the
/// keyboard to one there puts it on a handle outside the frame, which sends
/// every key to gpui's root and past the recovery controls the screen is
/// there to offer — the exact failure the list exists to end, reintroduced at
/// its own head.
///
/// Pure, and separate from the handing-over, because this is the part with
/// cases and the other is four lines of `window.focus`.
fn keyboard_owner_for(
    ringing_call: Option<String>,
    surfaces: KeyboardSurfaces,
    intent: ChatOpen,
    showing_settings: bool,
) -> KeyboardOwner {
    let composing = intent == ChatOpen::ToCompose;
    match ringing_call.filter(|_| surfaces.call_card) {
        Some(call_id) => KeyboardOwner::RingingCall(call_id),
        None if surfaces.viewer => KeyboardOwner::Viewer,
        None if showing_settings => KeyboardOwner::Screen,
        // Where both are drawn, the gesture decides. A chat opened to be
        // talked to hands the caret over; one opened to be looked at leaves
        // the list holding the arrow keys it is being walked with.
        None if surfaces.composer && composing => KeyboardOwner::Composer,
        None if surfaces.chat_list => KeyboardOwner::ChatList,
        // A phone drawing a conversation is not drawing its list, so there is
        // nowhere else for it to go — whatever the gesture was.
        None if surfaces.composer => KeyboardOwner::Composer,
        None => KeyboardOwner::Root,
    }
}

#[cfg(test)]
mod keyboard_owner_tests {
    use super::*;

    const NOTHING: KeyboardSurfaces = KeyboardSurfaces {
        chat_list: false,
        composer: false,
        viewer: false,
        call_card: false,
    };

    fn owner(surfaces: KeyboardSurfaces) -> KeyboardOwner {
        keyboard_owner_for(None, surfaces, ChatOpen::ToCompose, false)
    }

    /// The floor. There is no frame whose answer is "nobody", because the
    /// window itself is always drawn — and a window with no focus is one
    /// whose every shortcut is dead until something else gives it one.
    #[test]
    fn a_frame_that_draws_no_surface_still_has_an_owner() {
        assert_eq!(owner(NOTHING), KeyboardOwner::Root);
    }

    #[test]
    fn the_surfaces_are_preferred_in_order() {
        let all = KeyboardSurfaces {
            chat_list: true,
            composer: true,
            viewer: true,
            call_card: true,
        };
        assert_eq!(
            keyboard_owner_for(Some("call-1".into()), all, ChatOpen::ToCompose, true),
            KeyboardOwner::RingingCall("call-1".into())
        );
        assert_eq!(
            keyboard_owner_for(None, all, ChatOpen::ToCompose, true),
            KeyboardOwner::Viewer,
            "a picture outranks a screen that focuses nothing of its own"
        );
        assert_eq!(
            owner(KeyboardSurfaces {
                viewer: false,
                ..all
            }),
            KeyboardOwner::Composer
        );
        assert_eq!(
            owner(KeyboardSurfaces {
                chat_list: true,
                ..NOTHING
            }),
            KeyboardOwner::ChatList
        );
    }

    /// A viewer and a call both outlive the connection that opened them, and
    /// the error screen draws neither. Focus follows the frame, not the
    /// state.
    #[test]
    fn an_overlay_the_frame_did_not_draw_does_not_take_the_keyboard() {
        assert_eq!(
            keyboard_owner_for(Some("call-1".into()), NOTHING, ChatOpen::ToPreview, false),
            KeyboardOwner::Root,
            "a call ringing behind an error screen has no card to focus"
        );
        assert_eq!(
            keyboard_owner_for(Some("call-1".into()), NOTHING, ChatOpen::ToPreview, true),
            KeyboardOwner::Screen
        );
    }

    /// The composer exists as an entity long after the conversation that made
    /// it left the screen, and the offline strip replaces it in place.
    #[test]
    fn a_composer_that_is_not_drawn_is_not_the_owner() {
        assert_eq!(
            owner(KeyboardSurfaces {
                chat_list: true,
                ..NOTHING
            }),
            KeyboardOwner::ChatList
        );
    }

    /// Walking the list with the arrow keys selects chats, and every one of
    /// them draws a composer. The list's bindings are scoped to the list, so
    /// a composer that took the keyboard on selection ended the walk after
    /// one step.
    #[test]
    fn previewing_a_chat_leaves_the_arrow_keys_with_the_list() {
        let both = KeyboardSurfaces {
            chat_list: true,
            composer: true,
            ..NOTHING
        };
        assert_eq!(
            keyboard_owner_for(None, both, ChatOpen::ToPreview, false),
            KeyboardOwner::ChatList
        );
        assert_eq!(
            keyboard_owner_for(None, both, ChatOpen::ToCompose, false),
            KeyboardOwner::Composer,
            "opening a chat to talk to it still hands the caret over"
        );
    }

    /// A phone drawing a conversation is not drawing its list.
    #[test]
    fn a_preview_with_no_list_on_screen_still_lands_somewhere() {
        assert_eq!(
            keyboard_owner_for(
                None,
                KeyboardSurfaces {
                    composer: true,
                    ..NOTHING
                },
                ChatOpen::ToPreview,
                false
            ),
            KeyboardOwner::Composer
        );
    }
}

#[cfg(test)]
mod call_tests {
    use super::*;
    use oxidezap_core::VideoStream;

    /// A [`Calls`] with a live call on the stage and no window behind it.
    ///
    /// Everything below drives the entity's own state machine, which is the
    /// half that has no `Context` in it — what a camera is showing, what an
    /// ended call is written down as, and which pane may still hold a
    /// picture.
    fn in_a_call() -> Calls {
        let mut calls = Calls::new();
        calls.state.set_outgoing(OutgoingCall::new(
            "call-1".to_string(),
            "111@s.whatsapp.net".to_string(),
            "Peer".to_string(),
            true,
        ));
        assert!(calls.state.connect(&"call-1".to_string()));
        calls
    }

    /// The optimistic overlay: a camera takes seconds to open, and a control
    /// that stayed "off" for all of them reads as a click that did nothing.
    #[test]
    fn what_was_asked_for_outranks_what_the_camera_says() {
        let mut calls = in_a_call();
        assert!(!calls.video_showing(), "no camera is on yet");

        calls.video_asked = Some(("call-1".to_string(), true));

        assert!(
            calls.video_showing(),
            "the ask is what the button draws until the daemon answers"
        );
    }

    /// An ask outlives nothing: a call that has gone answers it, and an ask
    /// belonging to another call must not paint over this one's camera.
    #[test]
    fn an_ask_for_another_call_says_nothing_about_this_one() {
        let mut calls = in_a_call();
        calls
            .state
            .set_video(&"call-1".to_string(), VideoStream::Local, true);
        calls.video_asked = Some(("call-9".to_string(), false));

        assert!(
            calls.video_showing(),
            "the state is the answer where the ask is about a different call"
        );
    }

    /// `Some(None)` is "write it down, and let the stage say how", which is
    /// not the same answer as `None` — that is the daemon saying this device
    /// has no truthful record to write at all.
    #[test]
    fn a_call_that_left_is_written_down_only_when_the_state_says_so() {
        let mut state = CallState::new();
        assert_eq!(
            Calls::ending(&state, "call-1"),
            Some(None),
            "nothing said means the stage decides"
        );

        state.mark_ended_as(&"call-1".to_string(), CallOutcome::Declined);
        assert_eq!(
            Calls::ending(&state, "call-1"),
            Some(Some(CallOutcome::Declined)),
            "a refusal is not a missed call"
        );

        state.mark_unrecorded(&"call-1".to_string());
        assert_eq!(
            Calls::ending(&state, "call-1"),
            None,
            "answered on another device: this one has nothing to write"
        );
    }

    /// A camera that is switched off simply stops sending, so a pane left
    /// holding its last frame is a photograph of somebody who has gone.
    #[test]
    fn a_pane_whose_camera_went_off_stops_drawing() {
        let mut calls = in_a_call();
        let id = "call-1".to_string();
        calls.state.set_video(&id, VideoStream::Local, true);
        calls.state.set_video(&id, VideoStream::Remote, true);
        calls.pictures.call_id = Some(id.clone());
        calls.pictures.local = Some(Arc::new(image()));
        calls.pictures.remote = Some(Arc::new(image()));

        calls.state.set_video(&id, VideoStream::Remote, false);
        calls.pictures.follow(&calls.state);

        assert!(calls.pictures.of(VideoStream::Local).is_some());
        assert!(
            calls.pictures.of(VideoStream::Remote).is_none(),
            "the peer turned their camera off"
        );
    }

    /// A picture belongs to the call it was taken in. The socket is one hop
    /// behind the state, so a frame can arrive after the call it came from
    /// has ended — and drawing it into the next one puts the last person's
    /// face on this one.
    #[test]
    fn a_picture_from_a_call_that_ended_is_not_drawn_into_the_next_one() {
        let mut pictures = CallPictures::default();
        pictures.accept(CallFrame {
            call_id: "call-1".to_string(),
            stream: VideoStream::Remote,
            image: Arc::new(image()),
        });
        assert!(pictures.of(VideoStream::Remote).is_some());

        pictures.accept(CallFrame {
            call_id: "call-2".to_string(),
            stream: VideoStream::Local,
            image: Arc::new(image()),
        });

        assert!(
            pictures.of(VideoStream::Remote).is_none(),
            "the previous call's frame went with it"
        );
        assert!(pictures.of(VideoStream::Local).is_some());
    }

    /// One pixel, which is all any of this needs: the tests are about which
    /// pane holds a picture, never about what is in it.
    fn image() -> RenderImage {
        RenderImage::new(smallvec::SmallVec::from_elem(
            image::Frame::new(image::RgbaImage::new(1, 1)),
            1,
        ))
    }
}
