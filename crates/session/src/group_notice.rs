//! Turning a group notification into a sentence.
//!
//! The library reports a group change as a structured action; the timeline
//! needs a line a reader recognises. Only the changes a member would notice
//! get one — a rename, someone joining or leaving, an admin change, the
//! settings that decide who may speak or edit. The rest (invite-link
//! bookkeeping, membership-request plumbing) happens *to* the group without
//! being news in it, and a row for each would bury the conversation.

use whatsapp_rust::wacore::stanza::groups::{GroupNotificationAction, GroupParticipantInfo};
use whatsapp_rust::wacore_binary::jid::Jid;

/// What to say about `action`, or `None` when it is not worth a row.
///
/// `actor` is whoever triggered the change, already resolved to a name where
/// one is known.
pub fn describe(action: &GroupNotificationAction, actor: Option<&str>) -> Option<String> {
    let who = actor.unwrap_or("Someone");
    Some(match action {
        GroupNotificationAction::Add { participants, .. } => {
            format!("{who} added {}", names(participants))
        }
        // Leaving and being removed read very differently, and the difference
        // is whether the actor is the only participant named.
        GroupNotificationAction::Remove { participants, .. } => {
            if let [only] = participants.as_slice()
                && actor.is_some_and(|actor| actor == name_of(only))
            {
                format!("{who} left")
            } else {
                format!("{who} removed {}", names(participants))
            }
        }
        GroupNotificationAction::Promote { participants } => {
            format!("{who} made {} an admin", names(participants))
        }
        GroupNotificationAction::Demote { participants } => {
            format!("{who} removed {} as admin", names(participants))
        }
        GroupNotificationAction::Subject { subject, .. } => {
            format!("{who} changed the group name to \"{subject}\"")
        }
        GroupNotificationAction::Description { description, .. } => match description {
            Some(_) => format!("{who} changed the group description"),
            None => format!("{who} removed the group description"),
        },
        GroupNotificationAction::Locked { .. } => {
            format!("{who} restricted editing the group info to admins")
        }
        GroupNotificationAction::Unlocked => {
            format!("{who} allowed all members to edit the group info")
        }
        GroupNotificationAction::Announce => {
            format!("{who} restricted messages to admins")
        }
        GroupNotificationAction::NotAnnounce => {
            format!("{who} allowed all members to send messages")
        }
        GroupNotificationAction::Ephemeral { expiration, .. } => match expiration {
            0 => format!("{who} turned off disappearing messages"),
            secs => format!(
                "{who} set disappearing messages to {}",
                humanize_duration(*secs)
            ),
        },
        // Everything else is bookkeeping rather than news.
        _ => return None,
    })
}

/// A participant list as a reader would say it.
fn names(participants: &[GroupParticipantInfo]) -> String {
    match participants {
        [] => "someone".to_string(),
        [one] => name_of(one).to_string(),
        [first, second] => format!("{} and {}", name_of(first), name_of(second)),
        // Past two, counting is more use than reciting.
        [first, rest @ ..] => format!("{} and {} others", name_of(first), rest.len()),
    }
}

/// A participant's display name, falling back to their number.
fn name_of(participant: &GroupParticipantInfo) -> &str {
    participant
        .display_name
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or(participant.jid.user.as_str())
}

/// Whoever triggered the change, as a name rather than a JID.
///
/// The server's own label first, then the number. Not "You" for the reader's
/// own changes: nothing in this process knows which account is linked — the
/// device identity lives behind the daemon — and guessing at it would be
/// worse than naming everyone the same way.
pub fn actor_name(participant: Option<&Jid>, participant_username: Option<&str>) -> Option<String> {
    let participant = participant?;
    participant_username
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| Some(participant.user.to_string()))
}

/// A disappearing-message window, in the units WhatsApp offers it in.
fn humanize_duration(secs: u32) -> String {
    const DAY: u32 = 86_400;
    match secs {
        s if s % (7 * DAY) == 0 => plural(s / (7 * DAY), "week"),
        s if s % DAY == 0 => plural(s / DAY, "day"),
        s if s % 3_600 == 0 => plural(s / 3_600, "hour"),
        s => plural(s.div_ceil(60), "minute"),
    }
}

fn plural(count: u32, unit: &str) -> String {
    if count == 1 {
        format!("1 {unit}")
    } else {
        format!("{count} {unit}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn participant(user: &str, display: Option<&str>) -> GroupParticipantInfo {
        GroupParticipantInfo {
            jid: format!("{user}@s.whatsapp.net").parse().expect("valid jid"),
            phone_number: None,
            display_name: display.map(str::to_string),
            r#type: None,
            lid: None,
            username: None,
            join_time: None,
            group_history_sent_state: None,
        }
    }

    #[test]
    fn a_rename_quotes_the_new_name() {
        let action = GroupNotificationAction::Subject {
            subject: "Trip".to_string(),
            subject_owner: None,
            subject_time: None,
        };
        assert_eq!(
            describe(&action, Some("Ana")).as_deref(),
            Some("Ana changed the group name to \"Trip\"")
        );
    }

    #[test]
    fn removing_only_yourself_is_leaving() {
        let action = GroupNotificationAction::Remove {
            participants: vec![participant("1", Some("Ana"))],
            reason: None,
        };
        assert_eq!(describe(&action, Some("Ana")).as_deref(), Some("Ana left"));
    }

    #[test]
    fn removing_someone_else_is_a_removal() {
        let action = GroupNotificationAction::Remove {
            participants: vec![participant("2", Some("Bruno"))],
            reason: None,
        };
        assert_eq!(
            describe(&action, Some("Ana")).as_deref(),
            Some("Ana removed Bruno")
        );
    }

    #[test]
    fn a_crowd_is_counted_rather_than_recited() {
        let action = GroupNotificationAction::Add {
            participants: vec![
                participant("1", Some("Ana")),
                participant("2", Some("Bruno")),
                participant("3", Some("Cris")),
            ],
            reason: None,
        };
        assert_eq!(
            describe(&action, Some("You")).as_deref(),
            Some("You added Ana and 2 others")
        );
    }

    #[test]
    fn an_unnamed_participant_falls_back_to_their_number() {
        let action = GroupNotificationAction::Add {
            participants: vec![participant("5511999", None)],
            reason: None,
        };
        assert_eq!(
            describe(&action, None).as_deref(),
            Some("Someone added 5511999")
        );
    }

    #[test]
    fn bookkeeping_gets_no_row() {
        let action = GroupNotificationAction::RevokeInvite;
        assert!(describe(&action, Some("Ana")).is_none());
    }

    #[test]
    fn disappearing_windows_read_in_whole_units() {
        assert_eq!(humanize_duration(86_400), "1 day");
        assert_eq!(humanize_duration(7 * 86_400), "1 week");
        assert_eq!(humanize_duration(90 * 86_400), "90 days");
        assert_eq!(humanize_duration(3_600), "1 hour");
    }

    #[test]
    fn an_actor_is_named_by_the_server_label_or_their_number() {
        let actor: Jid = "5511999@s.whatsapp.net".parse().unwrap();
        assert_eq!(
            actor_name(Some(&actor), Some("ana")).as_deref(),
            Some("ana")
        );
        assert_eq!(
            actor_name(Some(&actor), None).as_deref(),
            Some("5511999"),
            "no label is still someone"
        );
        assert!(actor_name(None, Some("ana")).is_none());
    }
}
