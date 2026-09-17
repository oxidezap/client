//! The call history the wire reports.
//!
//! The store keeps no call log — calls are live state, not rows — so the
//! history is what this daemon has seen since it started: finalized calls
//! with their outcome and duration, newest last, bounded. Anything still
//! ringing is not history yet; the live stage answers for it through the
//! usual call queries.

use std::collections::{HashMap, VecDeque};

use wacore_binary::jid::{Jid, JidExt as _};

use oxidezap_core::UiEvent;
use oxidezap_wire::dto::CallEventDto;

use crate::state::StateHub;

/// How many finalized calls the ring keeps. A log, not an archive: the
/// CLI pages it, and anything older than this is gone with the process.
const RING_CAPACITY: usize = 100;

/// A call in flight, waiting for its outcome.
struct InterimCall {
    chat_jid: String,
    caller_jid: String,
    is_video: bool,
    is_group: bool,
    incoming: bool,
    started_ms: u64,
    connected_ms: Option<u64>,
}

/// Finalized calls with their outcome, newest last.
pub(crate) struct CallLog {
    interim: HashMap<String, InterimCall>,
    entries: VecDeque<CallEventDto>,
}

impl CallLog {
    pub(crate) fn new() -> Self {
        Self {
            interim: HashMap::new(),
            entries: VecDeque::new(),
        }
    }

    /// Fold one session event into the log. Runs before the hub folds the
    /// same event: what is recorded here is what happened, and the hub's
    /// post-state would only say what remains.
    pub(crate) fn observe(&mut self, event: &UiEvent, hub: &StateHub) {
        match event {
            UiEvent::IncomingCall(call) => {
                let chat_jid = call.caller_jid.clone();
                self.interim.insert(
                    call.call_id.clone(),
                    InterimCall {
                        chat_jid,
                        caller_jid: call.caller_jid.clone(),
                        is_video: call.is_video,
                        is_group: is_group_jid(&call.caller_jid),
                        incoming: true,
                        started_ms: now_ms(),
                        connected_ms: None,
                    },
                );
            }
            UiEvent::OutgoingCallStarted {
                call_id,
                recipient_jid,
                is_video,
                ..
            } => {
                let caller_jid = own_jid(hub).unwrap_or_default();
                self.interim.insert(
                    call_id.clone(),
                    InterimCall {
                        chat_jid: recipient_jid.clone(),
                        caller_jid,
                        is_video: *is_video,
                        is_group: is_group_jid(recipient_jid),
                        incoming: false,
                        started_ms: now_ms(),
                        connected_ms: None,
                    },
                );
            }
            UiEvent::CallAccepted(call_id) => {
                if let Some(call) = self.interim.get_mut(call_id) {
                    call.connected_ms.get_or_insert(now_ms());
                }
            }
            UiEvent::CallAnswered { call_id, is_video } => {
                if let Some(call) = self.interim.get_mut(call_id) {
                    call.connected_ms.get_or_insert(now_ms());
                    call.is_video = *is_video;
                }
            }
            UiEvent::CallEnded(call_id) => {
                if let Some(call) = self.interim.remove(call_id) {
                    self.push(finalize(call_id, call));
                }
            }
            // Answered or refused on another device, or never a call at
            // all: the event itself says there is nothing honest to write.
            UiEvent::CallEndedElsewhere(call_id) | UiEvent::CallUnrecorded(call_id) => {
                self.interim.remove(call_id);
            }
            UiEvent::OutgoingCallFailed { recipient_jid, .. } => {
                let key = self
                    .interim
                    .iter()
                    .find(|(_, call)| !call.incoming && &call.chat_jid == recipient_jid)
                    .map(|(id, _)| id.clone());
                if let Some(key) = key
                    && let Some(call) = self.interim.remove(&key)
                {
                    self.push(CallEventDto {
                        id: key,
                        outcome: "cancelled".to_string(),
                        ..dto_of(call, None)
                    });
                }
            }
            _ => {}
        }
    }

    /// The finalized calls, newest first, bounded by the ask.
    pub(crate) fn list(&self, limit: usize) -> Vec<CallEventDto> {
        self.entries
            .iter()
            .rev()
            .take(limit.max(1))
            .cloned()
            .collect()
    }

    fn push(&mut self, entry: CallEventDto) {
        self.entries.push_back(entry);
        while self.entries.len() > RING_CAPACITY {
            self.entries.pop_front();
        }
    }
}

/// An ended call as the protocol's call event.
fn finalize(id: &str, call: InterimCall) -> CallEventDto {
    let (outcome, duration) = match (call.incoming, call.connected_ms) {
        (_, Some(connected)) => {
            let elapsed = now_ms().saturating_sub(connected) / 1_000;
            ("connected", Some(elapsed.min(u64::from(u32::MAX)) as u32))
        }
        (true, None) => ("missed", None),
        (false, None) => ("cancelled", None),
    };
    CallEventDto {
        id: id.to_string(),
        outcome: outcome.to_string(),
        ..dto_of(call, duration)
    }
}

fn dto_of(call: InterimCall, duration_seconds: Option<u32>) -> CallEventDto {
    CallEventDto {
        id: String::new(),
        chat_jid: call.chat_jid,
        caller_jid: call.caller_jid,
        timestamp_ms: call.started_ms as i64,
        is_video: call.is_video,
        is_group: call.is_group,
        duration_seconds,
        outcome: String::new(),
    }
}

fn is_group_jid(jid: &str) -> bool {
    jid.parse::<Jid>().is_ok_and(|j| j.is_group())
}

/// This account's own JID, for the caller side of outgoing calls.
fn own_jid(hub: &StateHub) -> Option<String> {
    hub.snapshot().account?.jid.clone()
}

fn now_ms() -> u64 {
    wacore::time::now_millis().max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidezap_core::IncomingCall;

    fn incoming(id: &str) -> UiEvent {
        UiEvent::IncomingCall(IncomingCall {
            call_id: id.to_string(),
            caller_name: "Maria".to_string(),
            caller_jid: "559900000001@s.whatsapp.net".to_string(),
            is_video: false,
            is_offline: false,
            received_at: wacore::time::now_utc(),
        })
    }

    /// A missed call lands in the log with its outcome, newest first.
    #[test]
    fn a_missed_call_is_logged() {
        let hub = StateHub::new();
        let mut log = CallLog::new();
        log.observe(&incoming("c1"), &hub);
        assert!(log.list(10).is_empty(), "ringing is not history yet");
        log.observe(&UiEvent::CallEnded("c1".to_string()), &hub);
        let entries = log.list(10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].outcome, "missed");
        assert_eq!(entries[0].caller_jid, "559900000001@s.whatsapp.net");
        assert_eq!(entries[0].duration_seconds, None);
    }

    /// An answered call is connected, with a duration.
    #[test]
    fn an_answered_call_is_connected() {
        let hub = StateHub::new();
        let mut log = CallLog::new();
        log.observe(
            &UiEvent::OutgoingCallStarted {
                call_id: "c2".to_string(),
                recipient_jid: "559900000002@s.whatsapp.net".to_string(),
                placeholder_id: "p2".to_string(),
                is_video: true,
            },
            &hub,
        );
        log.observe(&UiEvent::CallAccepted("c2".to_string()), &hub);
        log.observe(&UiEvent::CallEnded("c2".to_string()), &hub);
        let entries = log.list(10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].outcome, "connected");
        assert!(entries[0].is_video);
        assert_eq!(entries[0].duration_seconds, Some(0));
    }

    /// Calls taken elsewhere leave no entry: this device has nothing true
    /// to write down.
    #[test]
    fn elsewhere_and_unrecorded_calls_leave_no_entry() {
        let hub = StateHub::new();
        let mut log = CallLog::new();
        log.observe(&incoming("c3"), &hub);
        log.observe(&UiEvent::CallEndedElsewhere("c3".to_string()), &hub);
        log.observe(&incoming("c4"), &hub);
        log.observe(&UiEvent::CallUnrecorded("c4".to_string()), &hub);
        assert!(log.list(10).is_empty());
    }

    /// The ring is bounded and newest-first.
    #[test]
    fn the_ring_is_bounded_and_newest_first() {
        let hub = StateHub::new();
        let mut log = CallLog::new();
        for n in 0..(RING_CAPACITY + 5) {
            let id = format!("c{n}");
            log.observe(&incoming(&id), &hub);
            log.observe(&UiEvent::CallEnded(id.clone()), &hub);
        }
        let entries = log.list(1000);
        assert_eq!(entries.len(), RING_CAPACITY);
        assert_eq!(entries[0].id, format!("c{}", RING_CAPACITY + 4));
    }
}
