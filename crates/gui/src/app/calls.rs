//! Call state.
//!
//! There is one call at a time and it moves through one sequence, so this is
//! a state machine rather than two independent `Option`s. The missing state
//! was the important one: accepting a call used to clear the incoming offer
//! and leave nothing behind, so the audio ran on with no duration, no mute and
//! no way to hang up. [`Stage::Active`] is that state.
//!
//! The card floats over the app rather than blocking it, so its position and
//! minimised flag outlive any single call — put it in a corner once and it
//! stays there.

use chrono::{DateTime, Utc};
use gpui::{Pixels, Point, px};
use oxidezap_core::{CallId, IncomingCall, OutgoingCall, OutgoingCallState};

/// A call that is connected and running.
#[derive(Debug, Clone)]
pub struct ActiveCall {
    pub call_id: CallId,
    pub peer_jid: String,
    pub peer_name: String,
    /// Whether the call was *offered* as video. The library is audio-only, so
    /// this shapes the card and nothing else; the video controls it reveals
    /// are drawn disabled.
    pub is_video: bool,
    pub started_at: DateTime<Utc>,
    pub muted: bool,
}

impl ActiveCall {
    /// How long the call has been up.
    ///
    /// Clamped at zero: `started_at` comes from the same clock the UI reads,
    /// but a clock that steps backwards would otherwise render a negative
    /// duration.
    pub fn elapsed(&self) -> chrono::Duration {
        (whatsapp_rust::wacore::time::now_utc() - self.started_at).max(chrono::Duration::zero())
    }

    /// `m:ss`, growing an hour field only once there is one.
    pub fn elapsed_label(&self) -> String {
        crate::app::chat_row::format_duration(self.elapsed().num_seconds().max(0) as u32)
    }

    pub fn initial(&self) -> char {
        self.peer_name.chars().next().unwrap_or('?')
    }
}

/// Where a call is in its life.
#[derive(Debug, Clone)]
pub enum Stage {
    /// Someone is calling; the card offers accept and decline.
    Incoming(IncomingCall),
    /// We are calling out; the card offers cancel.
    Outgoing(OutgoingCall),
    /// Connected.
    Active(ActiveCall),
}

impl Stage {
    /// The other party's JID, whichever stage we are in.
    pub fn peer_jid(&self) -> &str {
        match self {
            Self::Incoming(call) => &call.caller_jid,
            Self::Outgoing(call) => &call.recipient_jid,
            Self::Active(call) => &call.peer_jid,
        }
    }

    pub fn peer_name(&self) -> &str {
        match self {
            Self::Incoming(call) => &call.caller_name,
            Self::Outgoing(call) => &call.recipient_name,
            Self::Active(call) => &call.peer_name,
        }
    }

    pub fn call_id(&self) -> &str {
        match self {
            Self::Incoming(call) => &call.call_id,
            Self::Outgoing(call) => &call.call_id,
            Self::Active(call) => &call.call_id,
        }
    }

    pub fn is_video(&self) -> bool {
        match self {
            Self::Incoming(call) => call.is_video,
            Self::Outgoing(call) => call.is_video,
            Self::Active(call) => call.is_video,
        }
    }

    pub fn active(&self) -> Option<&ActiveCall> {
        match self {
            Self::Active(call) => Some(call),
            _ => None,
        }
    }

    /// Whether the call is still ringing, which is what makes Enter mean
    /// "accept" rather than "send".
    pub fn is_ringing(&self) -> bool {
        matches!(self, Self::Incoming(_) | Self::Outgoing(_))
    }
}

/// A second call arriving while one is up.
///
/// The old behaviour was a `warn!` and silence: the caller heard ringing that
/// the user never saw. Now it is surfaced so it can be refused deliberately.
#[derive(Debug, Clone)]
pub struct WaitingCall {
    pub call_id: CallId,
    pub caller_name: String,
}

/// The one call, plus the card's own presentation state.
#[derive(Default)]
pub struct CallState {
    stage: Option<Stage>,
    waiting: Option<WaitingCall>,
    /// Collapsed to a pill. Survives the call it was set during: a user who
    /// minimises every call means it.
    minimized: bool,
    /// Where the card was dragged to, as an offset from its default corner.
    /// Kept across calls for the same reason.
    offset: Point<Pixels>,
    /// Pointer position at the last drag sample. Dragging is applied as a
    /// running delta rather than from a remembered start point, so a dropped
    /// or coalesced move event costs one frame of lag instead of snapping the
    /// card to the pointer.
    drag_anchor: Option<Point<Pixels>>,
}

impl CallState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stage(&self) -> Option<&Stage> {
        self.stage.as_ref()
    }

    pub fn waiting(&self) -> Option<&WaitingCall> {
        self.waiting.as_ref()
    }

    pub fn incoming(&self) -> Option<&IncomingCall> {
        match &self.stage {
            Some(Stage::Incoming(call)) => Some(call),
            _ => None,
        }
    }

    pub fn outgoing(&self) -> Option<&OutgoingCall> {
        match &self.stage {
            Some(Stage::Outgoing(call)) => Some(call),
            _ => None,
        }
    }

    pub fn active(&self) -> Option<&ActiveCall> {
        self.stage.as_ref().and_then(Stage::active)
    }

    pub fn is_busy(&self) -> bool {
        self.stage.is_some()
    }

    pub fn is_minimized(&self) -> bool {
        self.minimized
    }

    pub fn offset(&self) -> Point<Pixels> {
        self.offset
    }

    pub fn set_minimized(&mut self, minimized: bool) {
        self.minimized = minimized;
    }

    pub fn drag_by(&mut self, delta: Point<Pixels>) {
        self.offset.x += delta.x;
        self.offset.y += delta.y;
    }

    /// The pointer went down on the drag handle.
    pub fn begin_drag(&mut self, at: Point<Pixels>) {
        self.drag_anchor = Some(at);
    }

    /// The pointer moved while dragging. Returns whether the card moved.
    pub fn drag_to(&mut self, at: Point<Pixels>) -> bool {
        let Some(anchor) = self.drag_anchor else {
            return false;
        };
        if at == anchor {
            return false;
        }
        self.drag_by(Point {
            x: at.x - anchor.x,
            y: at.y - anchor.y,
        });
        self.drag_anchor = Some(at);
        true
    }

    pub fn end_drag(&mut self) {
        self.drag_anchor = None;
    }

    pub fn is_dragging(&self) -> bool {
        self.drag_anchor.is_some()
    }

    /// Keep the card reachable after the window is resized smaller than the
    /// offset it was dragged to.
    pub fn clamp_offset(&mut self, limit: Point<Pixels>) {
        self.offset.x = self.offset.x.clamp(-limit.x, px(0.0));
        self.offset.y = self.offset.y.clamp(px(0.0), limit.y);
    }

    /// An offer arrived.
    ///
    /// Returns whether it became the current call. A second offer during a
    /// live call is parked in [`Self::waiting`] instead of clobbering the
    /// first, which would desync the UI from the call registry.
    pub fn set_incoming(&mut self, call: IncomingCall) -> bool {
        if self.stage.is_some() {
            self.waiting = Some(WaitingCall {
                call_id: call.call_id.clone(),
                caller_name: call.caller_name.clone(),
            });
            return false;
        }
        self.stage = Some(Stage::Incoming(call));
        true
    }

    /// We placed a call.
    pub fn set_outgoing(&mut self, call: OutgoingCall) {
        self.stage = Some(Stage::Outgoing(call));
    }

    /// Take the ringing offer, for accept or decline.
    pub fn take_incoming(&mut self) -> Option<IncomingCall> {
        match self.stage.take() {
            Some(Stage::Incoming(call)) => Some(call),
            other => {
                self.stage = other;
                None
            }
        }
    }

    pub fn take_outgoing(&mut self) -> Option<OutgoingCall> {
        match self.stage.take() {
            Some(Stage::Outgoing(call)) => Some(call),
            other => {
                self.stage = other;
                None
            }
        }
    }

    /// Take whatever call is up, for hanging up.
    pub fn take(&mut self) -> Option<Stage> {
        self.stage.take()
    }

    /// The call connected: this is the state that used to be missing.
    ///
    /// Accepts from either direction — we answered, or the peer answered us —
    /// and starts the clock now rather than when the offer arrived, so the
    /// duration counts talking time and not ringing time.
    pub fn connect(&mut self, call_id: &CallId) -> bool {
        let Some(stage) = self.stage.take() else {
            return false;
        };
        if stage.call_id() != call_id {
            self.stage = Some(stage);
            return false;
        }
        let active = ActiveCall {
            call_id: call_id.clone(),
            peer_jid: stage.peer_jid().to_string(),
            peer_name: stage.peer_name().to_string(),
            is_video: stage.is_video(),
            started_at: whatsapp_rust::wacore::time::now_utc(),
            muted: false,
        };
        self.stage = Some(Stage::Active(active));
        true
    }

    /// Accepting locally: the media is up before any peer answer arrives.
    pub fn connect_accepted(&mut self, call: &IncomingCall) {
        self.stage = Some(Stage::Active(ActiveCall {
            call_id: call.call_id.clone(),
            peer_jid: call.caller_jid.clone(),
            peer_name: call.caller_name.clone(),
            is_video: call.is_video,
            started_at: whatsapp_rust::wacore::time::now_utc(),
            muted: false,
        }));
    }

    /// Toggle the microphone, returning the new state.
    pub fn toggle_muted(&mut self) -> Option<bool> {
        match &mut self.stage {
            Some(Stage::Active(call)) => {
                call.muted = !call.muted;
                Some(call.muted)
            }
            _ => None,
        }
    }

    /// End whatever call carries `call_id`.
    ///
    /// Returns whether anything changed, so an ack for a call already gone
    /// does not buy a redraw.
    pub fn end(&mut self, call_id: &CallId) -> bool {
        if self.waiting.as_ref().is_some_and(|w| w.call_id == *call_id) {
            self.waiting = None;
            return true;
        }
        if self.stage.as_ref().is_some_and(|s| s.call_id() == call_id) {
            self.stage = None;
            // A card that was minimised for the last call should not silently
            // swallow the next one's ring.
            self.minimized = false;
            return true;
        }
        false
    }

    /// Clear the parked second call, once it has been refused.
    pub fn take_waiting(&mut self) -> Option<WaitingCall> {
        self.waiting.take()
    }

    /// The outgoing call reached the peer's device.
    pub fn set_outgoing_ringing(&mut self, call_id: &CallId) -> bool {
        match &mut self.stage {
            Some(Stage::Outgoing(call)) if call.call_id == *call_id => {
                call.set_state(OutgoingCallState::Ringing);
                true
            }
            _ => false,
        }
    }

    /// The placeholder id we invented is replaced by the server's real one.
    pub fn update_outgoing_call_id(&mut self, recipient_jid: &str, new_call_id: CallId) -> bool {
        match &mut self.stage {
            Some(Stage::Outgoing(call)) if call.recipient_jid == recipient_jid => {
                call.call_id = new_call_id;
                true
            }
            _ => false,
        }
    }

    /// The call to a recipient failed before it ever got an id.
    pub fn dismiss_outgoing_for_recipient(&mut self, recipient_jid: &str) -> bool {
        if matches!(&self.stage, Some(Stage::Outgoing(call)) if call.recipient_jid == recipient_jid)
        {
            self.stage = None;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn incoming(id: &str) -> IncomingCall {
        IncomingCall::new(
            id.to_string(),
            "Ana".to_string(),
            "a@s.whatsapp.net".to_string(),
            false,
            Arc::new(Default::default()),
        )
    }

    fn outgoing(id: &str) -> OutgoingCall {
        OutgoingCall::new(
            id.to_string(),
            "b@s.whatsapp.net".to_string(),
            "Bruno".to_string(),
            false,
        )
    }

    #[test]
    fn accepting_leaves_a_live_call_behind() {
        // The regression this whole state machine exists for: the UI used to
        // clear the offer and show nothing while the audio kept running.
        let mut state = CallState::new();
        let call = incoming("CALL");
        state.set_incoming(call.clone());
        let taken = state.take_incoming().expect("an offer was ringing");
        state.connect_accepted(&taken);

        let active = state.active().expect("the call is up");
        assert_eq!(active.call_id, "CALL");
        assert_eq!(active.peer_name, "Ana");
        assert!(!active.muted);
    }

    #[test]
    fn the_peer_answering_connects_an_outgoing_call() {
        let mut state = CallState::new();
        state.set_outgoing(outgoing("CALL"));
        assert!(state.connect(&"CALL".to_string()));
        assert!(state.active().is_some());
    }

    #[test]
    fn an_answer_for_a_different_call_is_ignored() {
        let mut state = CallState::new();
        state.set_outgoing(outgoing("CALL"));
        assert!(!state.connect(&"OTHER".to_string()));
        assert!(state.outgoing().is_some(), "the real call is untouched");
    }

    #[test]
    fn a_second_offer_waits_instead_of_replacing_the_live_call() {
        let mut state = CallState::new();
        state.set_incoming(incoming("FIRST"));
        assert!(!state.set_incoming(incoming("SECOND")));

        assert_eq!(state.stage().unwrap().call_id(), "FIRST");
        assert_eq!(state.waiting().unwrap().call_id, "SECOND");
    }

    #[test]
    fn refusing_the_waiting_call_leaves_the_first_alone() {
        let mut state = CallState::new();
        state.set_incoming(incoming("FIRST"));
        state.set_incoming(incoming("SECOND"));

        assert!(state.end(&"SECOND".to_string()));
        assert!(state.waiting().is_none());
        assert_eq!(state.stage().unwrap().call_id(), "FIRST");
    }

    #[test]
    fn muting_toggles_only_while_connected() {
        let mut state = CallState::new();
        state.set_incoming(incoming("CALL"));
        assert_eq!(state.toggle_muted(), None, "nothing to mute while ringing");

        let taken = state.take_incoming().unwrap();
        state.connect_accepted(&taken);
        assert_eq!(state.toggle_muted(), Some(true));
        assert_eq!(state.toggle_muted(), Some(false));
    }

    #[test]
    fn ending_an_unrelated_call_changes_nothing() {
        let mut state = CallState::new();
        state.set_incoming(incoming("CALL"));
        assert!(!state.end(&"OTHER".to_string()));
        assert!(state.is_busy());
    }

    #[test]
    fn a_minimised_card_reopens_for_the_next_call() {
        let mut state = CallState::new();
        state.set_incoming(incoming("CALL"));
        state.set_minimized(true);
        state.end(&"CALL".to_string());
        assert!(
            !state.is_minimized(),
            "the next call must not ring into a collapsed pill"
        );
    }

    #[test]
    fn the_dragged_position_outlives_the_call() {
        let mut state = CallState::new();
        state.drag_by(Point {
            x: px(-40.0),
            y: px(60.0),
        });
        state.set_incoming(incoming("CALL"));
        state.end(&"CALL".to_string());
        assert_eq!(state.offset().x, px(-40.0));
        assert_eq!(state.offset().y, px(60.0));
    }

    #[test]
    fn a_shrinking_window_pulls_the_card_back_into_view() {
        let mut state = CallState::new();
        state.drag_by(Point {
            x: px(-900.0),
            y: px(900.0),
        });
        state.clamp_offset(Point {
            x: px(300.0),
            y: px(200.0),
        });
        assert_eq!(state.offset().x, px(-300.0));
        assert_eq!(state.offset().y, px(200.0));
    }

    #[test]
    fn duration_counts_talking_time_not_ringing_time() {
        let mut state = CallState::new();
        state.set_incoming(incoming("CALL"));
        let taken = state.take_incoming().unwrap();
        state.connect_accepted(&taken);
        // Just connected, so the clock starts at zero rather than at whenever
        // the offer first arrived.
        assert_eq!(state.active().unwrap().elapsed_label(), "0:00");
    }
}
