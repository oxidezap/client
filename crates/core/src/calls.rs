//! Which calls are happening.
//!
//! Domain state, not view state: the transitions here are what a call *is* —
//! ringing, dialling, connected, gone — and the process holding the session
//! has to track them as closely as the one drawing them. Keeping one
//! implementation is also what lets a front end attaching mid-call be handed
//! this whole thing rather than a replay of events it missed.
//!
//! There is one call at a time and it moves through one sequence, so this is
//! a state machine rather than two independent `Option`s. The missing state
//! was the important one: accepting a call used to clear the incoming offer
//! and leave nothing behind, so the audio ran on with no duration, no mute and
//! no way to hang up. [`Stage::Active`] is that state.
//!
//! Where the card sits and whether it is collapsed is *not* here: that is one
//! window's presentation of this state, and a second front end attaching must
//! not inherit it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::call::{CallId, IncomingCall, OutgoingCall, OutgoingCallState};
use super::system_notice::format_duration;

/// A call that is connected and running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveCall {
    pub call_id: CallId,
    pub peer_jid: String,
    pub peer_name: String,
    /// Whether the call was *offered* as video. The library is audio-only, so
    /// this shapes the card and nothing else; the video controls it reveals
    /// are drawn disabled.
    pub is_video: bool,
    /// Whether this account placed the call.
    ///
    /// Carried through connecting rather than derived afterwards: once the
    /// stage becomes `Active` the direction is gone, and every completed call
    /// was recorded in the conversation as one we received.
    pub is_outgoing: bool,
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
        (wacore::time::now_utc() - self.started_at).max(chrono::Duration::zero())
    }

    /// `m:ss`, growing an hour field only once there is one.
    pub fn elapsed_label(&self) -> String {
        format_duration(self.elapsed().num_seconds().max(0) as u32)
    }

    pub fn initial(&self) -> char {
        self.peer_name.chars().next().unwrap_or('?')
    }
}

/// Where a call is in its life.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

    /// Whether *we* are the ones being called. An outgoing call is also
    /// ringing, but nothing about it can be accepted.
    pub fn is_incoming(&self) -> bool {
        matches!(self, Self::Incoming(_))
    }
}

/// A second call arriving while one is up.
///
/// The old behaviour was a `warn!` and silence: the caller heard ringing that
/// the user never saw. Now it is surfaced so it can be refused deliberately —
/// or answered, once the call in front of it ends.
///
/// The offer is kept whole rather than reduced to a name and an id, because
/// the caller is still ringing when the first call ends and what should happen
/// then is that their offer becomes *the* call. Rebuilding an `IncomingCall`
/// from a summary is not something this can do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingCall {
    call: IncomingCall,
}

impl WaitingCall {
    pub fn call_id(&self) -> &CallId {
        &self.call.call_id
    }

    pub fn caller_name(&self) -> &str {
        &self.call.caller_name
    }

    /// The parked offer itself, for a front end that wants to name its caller
    /// the way it names everyone else.
    pub fn call_mut(&mut self) -> &mut IncomingCall {
        &mut self.call
    }

    /// The offer, for a caller that is done with the wrapper — writing the
    /// refusal down needs the whole thing, not a name and an id.
    pub fn into_call(self) -> IncomingCall {
        self.call
    }
}

/// What became of an offer handed to [`CallState::set_incoming`].
///
/// Three outcomes and not a `bool`, because the third one obliges the caller
/// to do something: an offer that was neither taken nor parked is ringing at
/// somebody with nothing on this side holding its id, and the only honest
/// answer is to refuse it now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// It became the call on screen.
    Ringing,
    /// One call was already up, so it is parked behind it.
    Parked,
    /// A call was up and one was already parked. Nothing changed.
    Refused,
}

/// The one call, and whatever is queued behind it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallState {
    stage: Option<Stage>,
    waiting: Option<WaitingCall>,
    /// The last call this device has nothing truthful to write down about.
    ///
    /// Two things end that way: another of the account's devices took it —
    /// answered there or refused there — and a call the daemon would not
    /// place because one was already up, which a window had already drawn
    /// optimistically before it asked.
    ///
    /// Part of the state rather than an event beside it. A front end learns
    /// a call is over by watching the stage disappear, and it writes the
    /// conversation's record from the stage it last held; state and news
    /// travel on different channels, so an explanation sent alongside can
    /// arrive after the record it was meant to change. In the same frame as
    /// the removal it cannot.
    unrecorded: Option<CallId>,
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

    pub fn incoming_mut(&mut self) -> Option<&mut IncomingCall> {
        match &mut self.stage {
            Some(Stage::Incoming(call)) => Some(call),
            _ => None,
        }
    }

    pub fn waiting_mut(&mut self) -> Option<&mut IncomingCall> {
        self.waiting.as_mut().map(WaitingCall::call_mut)
    }

    pub fn outgoing_mut(&mut self) -> Option<&mut OutgoingCall> {
        match &mut self.stage {
            Some(Stage::Outgoing(call)) => Some(call),
            _ => None,
        }
    }

    pub fn active(&self) -> Option<&ActiveCall> {
        self.stage.as_ref().and_then(Stage::active)
    }

    pub fn has_incoming(&self) -> bool {
        self.incoming().is_some()
    }

    pub fn has_outgoing(&self) -> bool {
        self.outgoing().is_some()
    }

    /// Whether any call at all is up, which is what makes a new one wait.
    pub fn is_busy(&self) -> bool {
        self.stage.is_some()
    }

    /// An offer arrived.
    ///
    /// A second offer during a live call is parked in [`Self::waiting`]
    /// instead of clobbering the first, which would desync the UI from the
    /// call registry. A *third* is refused rather than displacing the parked
    /// one: there is exactly one waiting slot and exactly one strip drawing
    /// it, so overwriting left the displaced caller ringing in the session
    /// with no id anywhere on this side and no control to refuse them.
    ///
    /// The first offer wins the slot rather than the last, because that is the
    /// one the user has already been shown.
    pub fn set_incoming(&mut self, call: IncomingCall) -> Admission {
        let Some(stage) = &self.stage else {
            self.stage = Some(Stage::Incoming(call));
            return Admission::Ringing;
        };
        if let Some(parked) = &self.waiting {
            log::warn!(
                "refusing incoming call {}: {} is up and {} is already waiting",
                call.call_id,
                stage.call_id(),
                parked.call_id()
            );
            return Admission::Refused;
        }
        log::warn!(
            "parking incoming call {} behind {}",
            call.call_id,
            stage.call_id()
        );
        self.waiting = Some(WaitingCall { call });
        Admission::Parked
    }

    /// We placed a call.
    pub fn set_outgoing(&mut self, call: OutgoingCall) {
        if let Some(prev) = &self.stage {
            log::warn!(
                "replacing call {} with outgoing {}",
                prev.call_id(),
                call.call_id
            );
        }
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

    /// Take whatever call is up, for hanging up, and let whoever was parked
    /// behind it through.
    ///
    /// The promotion is not a nicety: with the stage empty and `waiting`
    /// still full, `is_busy` says no call is up while a caller is still
    /// ringing with nothing drawing them and no way to answer. It is also
    /// what the daemon does on the same gesture, so the optimistic state a
    /// window paints matches the one it is about to be sent.
    pub fn take(&mut self) -> Option<Stage> {
        let ended = self.stage.take();
        if ended.is_some() {
            self.promote_waiting();
        }
        ended
    }

    /// Give up on the call this device placed to `recipient_jid`.
    ///
    /// Named by recipient rather than by id because that is all a failure to
    /// place one carries: it never reached the wire, so it never got an id.
    ///
    /// Not `take_outgoing`, which hands the stage over to whatever is about
    /// to replace it. Nothing replaces this one, so it ends the way every
    /// other final removal does — letting whoever was parked behind it
    /// through. Without that, ringing someone while a second caller waits and
    /// failing to reach them left that caller in the state with no stage
    /// drawing them and no way to answer.
    pub fn fail_outgoing_to(&mut self, recipient_jid: &str) -> Option<OutgoingCall> {
        if !matches!(&self.stage, Some(Stage::Outgoing(call)) if call.recipient_jid == recipient_jid)
        {
            return None;
        }
        let failed = self.take_outgoing();
        self.promote_waiting();
        failed
    }

    /// Let the parked second offer onto the empty stage.
    ///
    /// The one place that decides it, because the rule is about the stage
    /// being empty rather than about how it emptied: with a caller in
    /// `waiting` and nothing on the stage, `is_busy` says no call is up while
    /// a phone is still ringing that nothing draws.
    fn promote_waiting(&mut self) {
        if self.stage.is_none() {
            self.stage = self
                .waiting
                .take()
                .map(|waiting| Stage::Incoming(waiting.call));
        }
    }

    /// Drop the ringing offer if it is the one named.
    ///
    /// Named calls only: an id for a call that is not the current one leaves
    /// the current one alone, which is what keeps a late ack for a call
    /// already gone from cancelling its successor.
    pub fn dismiss_incoming(&mut self, call_id: &CallId) -> bool {
        // A parked second offer is an incoming call too. Matching only the
        // stage left a refused caller sitting in the state, to be published
        // back to the front end that had just refused them.
        if self
            .waiting
            .as_ref()
            .is_some_and(|w| w.call_id() == call_id)
        {
            self.waiting = None;
            return true;
        }
        if matches!(&self.stage, Some(Stage::Incoming(call)) if call.call_id == *call_id) {
            self.stage = None;
            // The stage emptied, so whoever was parked behind it comes
            // forward — the same rule `take` and `end` follow. Refusing the
            // call on screen with a second one waiting otherwise left that
            // caller ringing with `is_busy` saying no call was up at all.
            self.promote_waiting();
            return true;
        }
        false
    }

    pub fn dismiss_outgoing(&mut self, call_id: &CallId) -> bool {
        if matches!(&self.stage, Some(Stage::Outgoing(call)) if call.call_id == *call_id) {
            self.stage = None;
            self.promote_waiting();
            return true;
        }
        false
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
        if matches!(stage, Stage::Active(_)) {
            // Already connected: a repeated accept must not restart the clock.
            self.stage = Some(stage);
            return false;
        }
        let active = ActiveCall {
            call_id: call_id.clone(),
            peer_jid: stage.peer_jid().to_string(),
            peer_name: stage.peer_name().to_string(),
            is_video: stage.is_video(),
            is_outgoing: matches!(stage, Stage::Outgoing(_)),
            started_at: wacore::time::now_utc(),
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
            // Answering an offer: they called us.
            is_outgoing: false,
            started_at: wacore::time::now_utc(),
            muted: false,
        }));
    }

    /// Set the microphone state, returning whether it changed.
    ///
    /// The daemon owns the device, so the front end asks and this records
    /// what was asked for; a toggle computed in one window would disagree
    /// with a second window watching the same call.
    pub fn set_muted(&mut self, call_id: &CallId, muted: bool) -> bool {
        match &mut self.stage {
            // The id has to match. A window that fell behind can ask to mute
            // a call that has already ended, and applying that to whatever
            // call is live *now* left the daemon's snapshot claiming a
            // microphone was muted while it was still open — the audio handle
            // is looked up by the stale id and never touched.
            Some(Stage::Active(call)) if call.call_id == *call_id && call.muted != muted => {
                call.muted = muted;
                true
            }
            _ => false,
        }
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
    /// [`end`](Self::end), for a call another of this account's devices
    /// answered or refused.
    ///
    /// The distinction is the whole point: nothing here was missed. The
    /// device that took the call has the real entry, and this one has no
    /// truthful record to write.
    pub fn end_elsewhere(&mut self, call_id: &CallId) -> bool {
        let ended = self.end(call_id);
        if ended {
            self.unrecorded = Some(call_id.clone());
        }
        ended
    }

    /// Say that `call_id` never happened, without ending anything.
    ///
    /// For a call a front end drew before asking and the daemon then refused:
    /// the stage it drew is not in this state and never was, so there is
    /// nothing to remove — only something to stop it writing down.
    pub fn mark_unrecorded(&mut self, call_id: &CallId) {
        self.unrecorded = Some(call_id.clone());
    }

    /// Whether `call_id` is one this device has no record to write.
    pub fn is_unrecorded(&self, call_id: &str) -> bool {
        self.unrecorded.as_deref() == Some(call_id)
    }

    pub fn end(&mut self, call_id: &CallId) -> bool {
        if self
            .waiting
            .as_ref()
            .is_some_and(|w| w.call_id() == call_id)
        {
            self.waiting = None;
            return true;
        }
        if self.stage.as_ref().is_some_and(|s| s.call_id() == call_id) {
            // Whoever was parked behind it is still ringing, and with the
            // stage empty nothing draws them — the card returns early with no
            // stage, so the caller would ring on with no way to answer or
            // refuse them. Ending the call in front promotes the one behind.
            self.stage = None;
            self.promote_waiting();
            return true;
        }
        false
    }

    /// Whether `stage` is still the call this state describes.
    ///
    /// Not the same question as [`holds`](Self::holds), because a call this
    /// device placed is drawn before it exists on the wire: it is given a
    /// local id, and the server's answer renames it. By id alone that rename
    /// reads as the call ending — so a front end wrote down an unanswered
    /// outgoing call for a call that was at that moment ringing, and wrote a
    /// second record when it really ended.
    ///
    /// An outgoing call is therefore matched by who it is *to*, which is the
    /// one thing the rename cannot change and the one thing a placed call has
    /// from the start.
    pub fn still_holds(&self, stage: &Stage) -> bool {
        if self.holds(stage.call_id()) {
            return true;
        }
        matches!(
            (stage, &self.stage),
            (Stage::Outgoing(mine), Some(Stage::Outgoing(theirs)))
                if mine.recipient_jid == theirs.recipient_jid
        )
    }

    /// Whether this state names `call_id` anywhere — as the stage or parked
    /// behind it.
    pub fn holds(&self, call_id: &str) -> bool {
        self.stage.as_ref().is_some_and(|s| s.call_id() == call_id)
            || self
                .waiting
                .as_ref()
                .is_some_and(|w| w.call_id() == call_id)
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

    /// The peer answered a call we placed.
    ///
    /// Kept as its own name because that is what the event says; it is
    /// [`Self::connect`] with the outgoing direction already known.
    pub fn set_outgoing_connected(&mut self, call_id: &CallId) -> bool {
        if !matches!(&self.stage, Some(Stage::Outgoing(call)) if call.call_id == *call_id) {
            return false;
        }
        self.connect(call_id)
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
    use wacore::types::call::{CallAction, IncomingCall as WaIncomingCall};
    use wacore_binary::jid::Jid;

    /// A minimal library offer.
    ///
    /// The state machine never reads it — only the daemon's voip facade does —
    /// but `IncomingCall::new` takes one, so the fixture has to produce it. It
    /// is `#[non_exhaustive]` with a builder, so this is the only way in.
    fn offer(id: &str) -> WaIncomingCall {
        let jid: Jid = "a@s.whatsapp.net".parse().expect("valid jid");
        WaIncomingCall::builder()
            .from(jid.clone())
            .stanza_id(id.to_string())
            .timestamp(wacore::time::now_utc())
            .offline(false)
            .action(CallAction::Offer {
                call_id: id.to_string(),
                call_creator: jid,
                caller_pn: None,
                caller_country_code: None,
                device_class: None,
                joinable: true,
                is_video: false,
                audio: Vec::new(),
                group_jid: None,
            })
            .build()
    }

    /// Every way the call on screen goes away, with someone parked behind
    /// it. Each of these clears the stage for good, so each has to let the
    /// waiting caller through — and a caller nothing draws is a caller with
    /// no Accept and no Decline.
    #[test]
    fn dismissing_the_call_in_front_promotes_whoever_was_waiting() {
        // Refusing the offer on screen while a second one waits behind it.
        let mut declined = CallState::default();
        declined.set_incoming(incoming("FIRST"));
        assert_eq!(declined.set_incoming(incoming("SECOND")), Admission::Parked);
        assert!(declined.dismiss_incoming(&"FIRST".to_string()));
        assert_eq!(
            declined.incoming().map(|c| c.call_id.as_str()),
            Some("SECOND")
        );
        assert!(declined.waiting().is_none());

        // Cancelling a call this device placed while one waits behind it.
        let mut cancelled = CallState::default();
        cancelled.set_outgoing(outgoing("MINE"));
        assert_eq!(
            cancelled.set_incoming(incoming("THEIRS")),
            Admission::Parked
        );
        assert!(cancelled.dismiss_outgoing(&"MINE".to_string()));
        assert_eq!(
            cancelled.incoming().map(|c| c.call_id.as_str()),
            Some("THEIRS")
        );
        assert!(cancelled.waiting().is_none());

        // And refusing the *parked* one leaves the call in front alone.
        let mut parked = CallState::default();
        parked.set_incoming(incoming("FIRST"));
        parked.connect(&"FIRST".to_string());
        assert_eq!(parked.set_incoming(incoming("SECOND")), Admission::Parked);
        assert!(parked.dismiss_incoming(&"SECOND".to_string()));
        assert_eq!(parked.active().map(|c| c.call_id.as_str()), Some("FIRST"));
        assert!(parked.waiting().is_none());
    }

    /// The two ways a call leaves without leaving a record. A front end
    /// writes the conversation's entry off the stage that disappeared, and
    /// disappearing is all it can see: a call answered on the phone would be
    /// written down as missed, and one the daemon refused to place as an
    /// attempt that was never made.
    #[test]
    fn a_call_can_end_with_nothing_to_write_down() {
        let mut state = CallState::new();
        state.set_incoming(incoming("call-1"));

        assert!(!state.is_unrecorded("call-1"), "nothing said yet");
        assert!(state.end_elsewhere(&"call-1".to_string()));
        assert!(state.is_unrecorded("call-1"));
        assert!(state.stage().is_none());

        // A refusal ends nothing here: the stage it is about was drawn in a
        // window and never reached this state at all.
        state.mark_unrecorded(&"ui-call-9".to_string());
        assert!(state.is_unrecorded("ui-call-9"));

        // And an ordinary ending still says nothing of the sort, which is
        // what keeps a genuine missed call counting as one.
        let mut missed = CallState::new();
        missed.set_incoming(incoming("call-2"));
        assert!(missed.end(&"call-2".to_string()));
        assert!(!missed.is_unrecorded("call-2"));
    }

    fn incoming(id: &str) -> IncomingCall {
        IncomingCall::new(
            id.to_string(),
            "Ana".to_string(),
            "a@s.whatsapp.net".to_string(),
            false,
            &offer(id),
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
    fn a_connected_call_remembers_who_placed_it() {
        let mut state = CallState::new();
        state.set_outgoing(outgoing("CALL"));
        state.connect(&"CALL".to_string());
        assert!(state.active().unwrap().is_outgoing, "we dialled");

        let mut state = CallState::new();
        state.set_incoming(incoming("CALL"));
        let taken = state.take_incoming().unwrap();
        state.connect_accepted(&taken);
        assert!(!state.active().unwrap().is_outgoing, "they called us");
    }

    #[test]
    fn connecting_twice_does_not_restart_the_clock() {
        let mut state = CallState::new();
        state.set_outgoing(outgoing("CALL"));
        assert!(state.connect(&"CALL".to_string()));
        let started = state.active().unwrap().started_at;
        assert!(!state.connect(&"CALL".to_string()));
        assert_eq!(state.active().unwrap().started_at, started);
    }

    #[test]
    fn a_second_offer_waits_instead_of_replacing_the_live_call() {
        let mut state = CallState::new();
        state.set_incoming(incoming("FIRST"));
        assert_eq!(
            state.set_incoming(incoming("SECOND")),
            Admission::Parked,
            "the live call keeps the stage"
        );

        assert_eq!(state.stage().unwrap().call_id(), "FIRST");
        assert_eq!(state.waiting().unwrap().call_id(), "SECOND");
    }

    /// There is one waiting slot and one strip drawing it. Overwriting left
    /// the displaced caller ringing in the session with no id anywhere on this
    /// side and no control to refuse them, which is exactly the state the
    /// waiting slot exists to prevent.
    #[test]
    fn a_third_offer_is_refused_rather_than_displacing_the_one_waiting() {
        let mut state = CallState::default();
        assert_eq!(state.set_incoming(incoming("FIRST")), Admission::Ringing);
        state.connect(&"FIRST".to_string());
        assert_eq!(state.set_incoming(incoming("SECOND")), Admission::Parked);

        assert_eq!(state.set_incoming(incoming("THIRD")), Admission::Refused);
        assert_eq!(
            state.waiting().unwrap().call_id(),
            "SECOND",
            "the caller the user was already shown keeps the slot"
        );
        assert!(!state.holds("THIRD"), "nothing on this side holds it");
    }

    /// A stale window can ask to mute a call that has already ended. Applying
    /// that to whatever call is live now makes the snapshot lie about a
    /// microphone the audio handle never touched.
    #[test]
    fn muting_a_call_that_ended_does_not_mute_its_successor() {
        let mut state = CallState::default();
        state.set_incoming(incoming("SECOND"));
        state.connect(&"SECOND".to_string());

        assert!(!state.set_muted(&"FIRST".to_string(), true));
        assert!(!state.active().unwrap().muted, "the live call is untouched");

        assert!(state.set_muted(&"SECOND".to_string(), true));
        assert!(state.active().unwrap().muted);
    }

    /// The parked caller is still ringing when the call in front of them
    /// ends. Nothing draws a waiting call on its own, so leaving it parked
    /// meant a caller with no way to be answered or refused.
    #[test]
    fn ending_a_call_promotes_whoever_was_waiting_behind_it() {
        let mut state = CallState::default();
        state.set_incoming(incoming("FIRST"));
        state.connect(&"FIRST".to_string());
        assert_eq!(state.set_incoming(incoming("SECOND")), Admission::Parked);

        assert!(state.end(&"FIRST".to_string()));
        assert_eq!(state.incoming().map(|c| c.call_id.as_str()), Some("SECOND"));
        assert!(
            state.waiting().is_none(),
            "it is the call now, not the queue"
        );
    }

    /// The same promotion, on the other way a stage can empty for good: a
    /// call this device never managed to place. `take_outgoing` cleared the
    /// stage and left the parked caller ringing behind an empty card.
    #[test]
    fn a_call_that_could_not_be_placed_promotes_whoever_was_waiting() {
        let mut state = CallState::default();
        state.set_outgoing(outgoing("MINE"));
        assert_eq!(state.set_incoming(incoming("THEIRS")), Admission::Parked);

        let failed = state
            .fail_outgoing_to("b@s.whatsapp.net")
            .expect("the call this device placed");
        assert_eq!(failed.call_id, "MINE");
        assert_eq!(state.incoming().map(|c| c.call_id.as_str()), Some("THEIRS"));
        assert!(state.waiting().is_none());
    }

    /// Named by recipient, and a failure for someone else is not this call's.
    #[test]
    fn a_failure_to_a_different_recipient_leaves_the_call_alone() {
        let mut state = CallState::default();
        state.set_outgoing(outgoing("MINE"));

        assert!(state.fail_outgoing_to("c@s.whatsapp.net").is_none());
        assert_eq!(state.outgoing().map(|c| c.call_id.as_str()), Some("MINE"));
    }

    /// The window draws the call it placed before the server has answered,
    /// under a local id. The answer renames it — and by id alone that read as
    /// the call ending, so the conversation got "outgoing, not answered" for
    /// a call that was ringing at that moment.
    #[test]
    fn a_call_being_given_its_real_id_has_not_ended() {
        let mut drawn = CallState::default();
        drawn.set_outgoing(outgoing("ui-call-1"));
        let stage = drawn.stage().expect("the call this window drew").clone();

        let mut named = CallState::default();
        named.set_outgoing(outgoing("REAL"));

        assert!(!named.holds(stage.call_id()), "the id really did change");
        assert!(
            named.still_holds(&stage),
            "but it is the same call to the same person"
        );

        let mut elsewhere = CallState::default();
        elsewhere.set_outgoing(OutgoingCall::new(
            "REAL".to_string(),
            "c@s.whatsapp.net".to_string(),
            "Carla".to_string(),
            false,
        ));
        assert!(
            !elsewhere.still_holds(&stage),
            "a call to someone else is a different call"
        );
    }

    #[test]
    fn a_state_knows_which_calls_it_names() {
        let mut state = CallState::default();
        state.set_incoming(incoming("FIRST"));
        state.connect(&"FIRST".to_string());
        state.set_incoming(incoming("SECOND"));

        assert!(state.holds("FIRST"));
        assert!(state.holds("SECOND"), "parked counts as held");
        assert!(!state.holds("THIRD"));
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

    /// Refusing the parked caller has to remove them from the state that
    /// gets published, or the next snapshot brings the strip back.
    #[test]
    fn dismissing_reaches_the_parked_call_too() {
        let mut state = CallState::new();
        state.set_incoming(incoming("FIRST"));
        state.set_incoming(incoming("SECOND"));

        assert!(state.dismiss_incoming(&"SECOND".to_string()));
        assert!(state.waiting().is_none());
        assert_eq!(
            state.stage().unwrap().call_id(),
            "FIRST",
            "the call on screen is untouched"
        );
    }

    #[test]
    fn dismissing_names_the_call_it_dismisses() {
        let mut state = CallState::new();
        state.set_incoming(incoming("CALL"));
        assert!(!state.dismiss_incoming(&"OTHER".to_string()));
        assert!(state.has_incoming(), "a stale ack cancels nothing");
        assert!(state.dismiss_incoming(&"CALL".to_string()));
        assert!(!state.is_busy());
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
    fn duration_counts_talking_time_not_ringing_time() {
        let mut state = CallState::new();
        state.set_incoming(incoming("CALL"));
        let taken = state.take_incoming().unwrap();
        state.connect_accepted(&taken);
        // Just connected, so the clock starts at zero rather than at whenever
        // the offer first arrived.
        assert_eq!(state.active().unwrap().elapsed_label(), "0:00");
    }

    /// The snapshot a front end attaching mid-call is handed.
    #[test]
    fn a_live_call_survives_the_wire() {
        let mut state = CallState::new();
        state.set_outgoing(outgoing("CALL"));
        state.connect(&"CALL".to_string());
        state.set_incoming(incoming("SECOND"));

        let json = serde_json::to_string(&state).expect("serializable");
        let back: CallState = serde_json::from_str(&json).expect("round trip");
        assert_eq!(back, state);
    }
}
