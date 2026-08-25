//! Things that happened in a conversation that nobody typed.

use serde::{Deserialize, Serialize};

/// How a call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallOutcome {
    /// Answered, and ran for this many seconds.
    Completed(u32),
    /// Rang out, or was never answered.
    Missed,
    /// Someone hung up on purpose before it connected.
    Declined,
}

/// A call, as it appears in the conversation afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallRecord {
    pub is_video: bool,
    /// Whether we placed it.
    pub is_outgoing: bool,
    pub outcome: CallOutcome,
}

impl CallRecord {
    /// Whether this is a call that rang here and nobody answered.
    ///
    /// The one call worth a badge. Everything else in this enum is an event
    /// the user was party to — they took it, placed it, or refused it — and a
    /// chat that badges itself for those is claiming there is something to
    /// catch up on when there is not.
    pub fn is_missed_inbound(&self) -> bool {
        !self.is_outgoing && matches!(self.outcome, CallOutcome::Missed)
    }

    /// The line describing what happened.
    pub fn title(&self) -> String {
        let kind = if self.is_video {
            "Video call"
        } else {
            "Voice call"
        };
        match self.outcome {
            // "Missed" is only true of a call that rang at us; one we placed
            // and nobody answered is not the reader's fault to be told about.
            CallOutcome::Missed if !self.is_outgoing => format!("Missed {}", kind.to_lowercase()),
            CallOutcome::Missed => format!("{kind}, no answer"),
            _ => kind.to_string(),
        }
    }

    /// One line for a chat row, where there is no room for two.
    ///
    /// The direction is dropped: the list is scanned rather than read, and
    /// "missed" or "no answer" already carries the part worth acting on.
    pub fn summary(&self) -> String {
        match self.outcome {
            CallOutcome::Completed(secs) => {
                format!("{} · {}", self.title(), format_duration(secs))
            }
            _ => self.title(),
        }
    }

    /// The second line: how long, or how to try again.
    pub fn detail(&self) -> String {
        let direction = if self.is_outgoing {
            "outgoing"
        } else {
            "incoming"
        };
        match self.outcome {
            CallOutcome::Completed(secs) => {
                format!("{direction} · {}", format_duration(secs))
            }
            CallOutcome::Missed => "tap to call back".to_string(),
            CallOutcome::Declined => format!("{direction} · declined"),
        }
    }

    /// Whether this record should read as something that needs attention.
    pub fn is_missed(&self) -> bool {
        matches!(self.outcome, CallOutcome::Missed) && !self.is_outgoing
    }
}

/// A conversation event with no author.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemNotice {
    Call(CallRecord),
    /// A group's name, picture or membership changed.
    GroupChanged(String),
}

/// `m:ss`, or `h:mm:ss` past an hour.
pub fn format_duration(total_secs: u32) -> String {
    let (hours, minutes, seconds) = (total_secs / 3600, (total_secs / 60) % 60, total_secs % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {

    /// A call you took, placed, or refused is not something to catch up on.
    /// It is written into the conversation as an incoming row all the same,
    /// which is what used to badge the chat for an event the user was in.
    #[test]
    fn only_a_call_that_rang_unanswered_is_worth_a_badge() {
        let call = |is_outgoing, outcome| CallRecord {
            is_video: false,
            is_outgoing,
            outcome,
        };

        assert!(call(false, CallOutcome::Missed).is_missed_inbound());

        for record in [
            call(false, CallOutcome::Completed(12)),
            call(false, CallOutcome::Declined),
            call(true, CallOutcome::Missed),
            call(true, CallOutcome::Completed(12)),
            call(true, CallOutcome::Declined),
        ] {
            assert!(
                !record.is_missed_inbound(),
                "{:?} is not a missed inbound call",
                record.outcome
            );
        }
    }
    use super::*;

    fn record(is_outgoing: bool, outcome: CallOutcome) -> CallRecord {
        CallRecord {
            is_video: false,
            is_outgoing,
            outcome,
        }
    }

    #[test]
    fn a_completed_call_reports_its_length_and_direction() {
        let call = record(false, CallOutcome::Completed(252));
        assert_eq!(call.title(), "Voice call");
        assert_eq!(call.detail(), "incoming · 4:12");
        assert!(!call.is_missed());
    }

    #[test]
    fn only_a_call_that_rang_at_us_counts_as_missed() {
        let inbound = record(false, CallOutcome::Missed);
        assert_eq!(inbound.title(), "Missed voice call");
        assert_eq!(inbound.detail(), "tap to call back");
        assert!(inbound.is_missed());

        // One we placed and nobody answered is not the reader's failure.
        let outbound = record(true, CallOutcome::Missed);
        assert_eq!(outbound.title(), "Voice call, no answer");
        assert!(!outbound.is_missed());
    }

    #[test]
    fn video_says_so() {
        let call = CallRecord {
            is_video: true,
            ..record(false, CallOutcome::Missed)
        };
        assert_eq!(call.title(), "Missed video call");
    }

    #[test]
    fn a_declined_call_says_which_way_it_went() {
        assert_eq!(
            record(true, CallOutcome::Declined).detail(),
            "outgoing · declined"
        );
    }
}
