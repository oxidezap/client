//! Call state structures for the UI.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use wacore::types::call::IncomingCall as WaIncomingCall;

/// Call ids are plain strings on the wire (and in the voip facade).
pub type CallId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutgoingCallState {
    Initiating,
    Ringing,
    Connected,
    Declined,
    Timeout,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingCall {
    pub call_id: CallId,
    pub recipient_name: String,
    pub recipient_jid: String,
    pub is_video: bool,
    pub state: OutgoingCallState,
    pub initiated_at: DateTime<Utc>,
}

impl OutgoingCall {
    pub fn new(
        call_id: impl Into<CallId>,
        recipient_jid: String,
        recipient_name: String,
        is_video: bool,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            recipient_name,
            recipient_jid,
            is_video,
            state: OutgoingCallState::Initiating,
            initiated_at: wacore::time::now_utc(),
        }
    }

    pub fn initial(&self) -> char {
        self.recipient_name.chars().next().unwrap_or('?')
    }

    pub fn set_state(&mut self, state: OutgoingCallState) {
        self.state = state;
    }

    pub fn is_active(&self) -> bool {
        !matches!(
            self.state,
            OutgoingCallState::Declined | OutgoingCallState::Timeout
        )
    }

    pub fn status_message(&self) -> &'static str {
        match self.state {
            OutgoingCallState::Initiating => "Calling...",
            OutgoingCallState::Ringing => "Ringing...",
            OutgoingCallState::Connected => "Connected",
            OutgoingCallState::Declined => "Call declined",
            OutgoingCallState::Timeout => "No answer",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncomingCall {
    pub call_id: CallId,
    pub caller_name: String,
    pub caller_jid: String,
    pub is_video: bool,
    pub is_offline: bool,
    pub received_at: DateTime<Utc>,
}

impl IncomingCall {
    /// Build a ringing call from the library's offer.
    ///
    /// The offer itself does not come along: whoever accepts or declines looks
    /// it up by call id in the session's own registry, so a copy here would be
    /// a second owner of a payload only one process can act on — and one that
    /// could not cross the daemon socket if it tried.
    pub fn new(
        call_id: impl Into<CallId>,
        caller_name: String,
        caller_jid: String,
        is_video: bool,
        offer: &WaIncomingCall,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            caller_name,
            caller_jid,
            is_video,
            is_offline: offer.offline,
            received_at: wacore::time::now_utc(),
        }
    }

    pub fn initial(&self) -> char {
        self.caller_name.chars().next().unwrap_or('?')
    }
}
