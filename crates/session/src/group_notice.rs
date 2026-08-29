//! Turning a group notification into a sentence.
//!
//! The library reports a group change as a structured action; the timeline
//! needs a line a reader recognises. Only the changes a member would notice
//! get one — a rename, someone joining or leaving, an admin change, the
//! settings that decide who may speak or edit. The rest (invite-link
//! bookkeeping, membership-request plumbing) happens *to* the group without
//! being news in it, and a row for each would bury the conversation.

use std::collections::HashMap;

use oxidezap_core::fallback_chat_name;
use whatsapp_rust::wacore::stanza::groups::{GroupNotificationAction, GroupParticipantInfo};
use whatsapp_rust::wacore_binary::jid::{Jid, JidExt as _};

/// Names the address book had for the people this notification mentions,
/// keyed by JID.
///
/// A notice is a sentence about people who also have bubbles and typing lines
/// on the same screen, and those are named by `session/names.rs`. Resolving is
/// async and this module is not, so the answers are looked up first and handed
/// in — the alternative is a group change naming somebody by the push name
/// they chose while every other surface calls them what the reader saved them
/// as.
pub type ResolvedNames = HashMap<String, String>;

/// Everyone `action` names, so they can be resolved before it is described.
pub fn participants_of(action: &GroupNotificationAction) -> &[GroupParticipantInfo] {
    match action {
        GroupNotificationAction::Add { participants, .. }
        | GroupNotificationAction::Remove { participants, .. }
        | GroupNotificationAction::Promote { participants }
        | GroupNotificationAction::Demote { participants } => participants,
        _ => &[],
    }
}

/// What to say about `action`, or `None` when it is not worth a row.
///
/// `actor` is whoever triggered the change, already resolved to a name where
/// one is known.
pub fn describe(
    action: &GroupNotificationAction,
    actor: Option<&str>,
    actor_jid: Option<&Jid>,
    named: &ResolvedNames,
) -> Option<String> {
    let who = actor.unwrap_or("Someone");
    Some(match action {
        GroupNotificationAction::Add { participants, .. } => {
            format!("{who} added {}", names(participants, named))
        }
        // Leaving and being removed read very differently, and the difference
        // is whether the actor is the only participant named.
        GroupNotificationAction::Remove { participants, .. } => {
            // Identity, not display name: two members can share a name, and an
            // admin removing their namesake would have read as "Ana left".
            if let [only] = participants.as_slice()
                && actor_jid.is_some_and(|actor| actor.is_same_user_as(&only.jid))
            {
                format!("{who} left")
            } else {
                format!("{who} removed {}", names(participants, named))
            }
        }
        GroupNotificationAction::Promote { participants } => {
            format!("{who} made {} an admin", names(participants, named))
        }
        GroupNotificationAction::Demote { participants } => {
            format!("{who} removed {} as admin", names(participants, named))
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
fn names(participants: &[GroupParticipantInfo], named: &ResolvedNames) -> String {
    match participants {
        [] => "someone".to_string(),
        [one] => name_of(one, named).to_string(),
        [first, second] => format!("{} and {}", name_of(first, named), name_of(second, named)),
        // Past two, counting is more use than reciting.
        [first, rest @ ..] => format!("{} and {} others", name_of(first, named), rest.len()),
    }
}

/// A participant's name: the one every other surface uses, then the label the
/// server attached, then their number.
fn name_of(participant: &GroupParticipantInfo, named: &ResolvedNames) -> String {
    named
        .get(&participant.jid.to_string())
        .cloned()
        .or_else(|| {
            participant
                .display_name
                .as_deref()
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
        })
        // The same last resort every other surface uses. The raw JID user was
        // a second rule living here: on a LID conversation it printed an
        // internal number that reads as a phone number, beside a bubble from
        // the same person saying "Unknown contact".
        .unwrap_or_else(|| fallback_chat_name(&participant.jid))
}

/// Whoever triggered the change, as a name rather than a JID.
///
/// The server's own label first, then the same last resort every other
/// surface uses. Not "You" for the reader's
/// own changes: nothing in this process knows which account is linked — the
/// device identity lives behind the daemon — and guessing at it would be
/// worse than naming everyone the same way.
pub fn actor_name(
    participant: Option<&Jid>,
    participant_username: Option<&str>,
    named: &ResolvedNames,
) -> Option<String> {
    let participant = participant?;
    named
        .get(&participant.to_string())
        .cloned()
        .or_else(|| {
            participant_username
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        })
        .or_else(|| Some(fallback_chat_name(participant)))
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

    fn participant_jid(jid: &str) -> GroupParticipantInfo {
        GroupParticipantInfo {
            jid: jid.parse().expect("valid jid"),
            phone_number: None,
            display_name: None,
            r#type: None,
            lid: None,
            username: None,
            join_time: None,
            group_history_sent_state: None,
        }
    }

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
            subject_owner_pn: None,
            subject_owner_username: None,
            subject_time: None,
        };
        assert_eq!(
            describe(&action, Some("Ana"), None, &ResolvedNames::new()).as_deref(),
            Some("Ana changed the group name to \"Trip\"")
        );
    }

    fn jid(user: &str) -> Jid {
        format!("{user}@s.whatsapp.net").parse().expect("valid jid")
    }

    #[test]
    fn removing_only_yourself_is_leaving() {
        let action = GroupNotificationAction::Remove {
            participants: vec![participant("1", Some("Ana"))],
            reason: None,
        };
        assert_eq!(
            describe(&action, Some("Ana"), Some(&jid("1")), &ResolvedNames::new()).as_deref(),
            Some("Ana left")
        );
    }

    #[test]
    fn removing_someone_else_is_a_removal() {
        let action = GroupNotificationAction::Remove {
            participants: vec![participant("2", Some("Bruno"))],
            reason: None,
        };
        assert_eq!(
            describe(&action, Some("Ana"), Some(&jid("1")), &ResolvedNames::new()).as_deref(),
            Some("Ana removed Bruno")
        );
    }

    /// Two members can share a display name, and an admin removing their
    /// namesake is not that namesake leaving.
    #[test]
    fn a_namesake_removal_is_not_a_departure() {
        let action = GroupNotificationAction::Remove {
            participants: vec![participant("2", Some("Ana"))],
            reason: None,
        };
        assert_eq!(
            describe(&action, Some("Ana"), Some(&jid("1")), &ResolvedNames::new()).as_deref(),
            Some("Ana removed Ana")
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
            describe(&action, Some("You"), None, &ResolvedNames::new()).as_deref(),
            Some("You added Ana and 2 others")
        );
    }

    #[test]
    fn an_unnamed_participant_falls_back_the_way_every_surface_does() {
        let action = GroupNotificationAction::Add {
            participants: vec![participant("5511999", None)],
            reason: None,
        };
        assert_eq!(
            describe(&action, None, None, &ResolvedNames::new()).as_deref(),
            Some("Someone added +5511999")
        );
    }

    /// The digits of a LID are not a phone number, and printing them beside a
    /// bubble that says "Unknown contact" is the same person under two names.
    #[test]
    fn an_unnamed_lid_participant_is_not_named_by_its_digits() {
        let action = GroupNotificationAction::Add {
            participants: vec![participant_jid("123456789012345@lid")],
            reason: None,
        };
        assert_eq!(
            describe(&action, None, None, &ResolvedNames::new()).as_deref(),
            Some("Someone added Unknown contact")
        );
    }

    #[test]
    fn bookkeeping_gets_no_row() {
        let action = GroupNotificationAction::RevokeInvite;
        assert!(describe(&action, Some("Ana"), None, &ResolvedNames::new()).is_none());
    }

    #[test]
    fn disappearing_windows_read_in_whole_units() {
        assert_eq!(humanize_duration(86_400), "1 day");
        assert_eq!(humanize_duration(7 * 86_400), "1 week");
        assert_eq!(humanize_duration(90 * 86_400), "90 days");
        assert_eq!(humanize_duration(3_600), "1 hour");
    }

    #[test]
    fn an_actor_is_named_by_the_server_label_or_the_usual_last_resort() {
        let actor: Jid = "5511999@s.whatsapp.net".parse().unwrap();
        assert_eq!(
            actor_name(Some(&actor), Some("ana"), &ResolvedNames::new()).as_deref(),
            Some("ana")
        );
        assert_eq!(
            actor_name(Some(&actor), None, &ResolvedNames::new()).as_deref(),
            Some("+5511999"),
            "no label is still someone"
        );
        assert!(actor_name(None, Some("ana"), &ResolvedNames::new()).is_none());
    }
}
