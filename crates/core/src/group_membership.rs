//! Membership facts retain wire identities; labels are refreshed from this account's cache.
use serde::{Deserialize, Serialize};
use wacore::stanza::groups::GroupNotificationAction;
use wacore::types::events::GroupUpdate;
use wacore_binary::jid::Jid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MembershipAction {
    Add,
    Remove,
    Promote,
    Demote,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticeParticipant {
    pub jid: String,
    pub phone_jid: Option<String>,
    pub lid: Option<String>,
    pub offered_name: Option<String>,
    /// A projection only. The store never writes hydrated labels back.
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub is_self: bool,
    #[serde(default)]
    pub canonical_jid: Option<String>,
}
impl NoticeParticipant {
    pub fn actor_of(update: &GroupUpdate) -> Option<Self> {
        update.participant.as_ref().map(|jid| NoticeParticipant {
            jid: jid.to_string(),
            phone_jid: update.participant_pn.as_ref().map(ToString::to_string),
            lid: None,
            offered_name: update.participant_username.clone(),
            label: None,
            is_self: false,
            canonical_jid: None,
        })
    }
    pub fn label(&self) -> String {
        if self.is_self {
            return "you".to_owned();
        }
        self.label
            .as_deref()
            .or(self.offered_name.as_deref())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| self.fallback_label())
    }
    pub fn fallback_label(&self) -> String {
        self.phone_jid
            .as_deref()
            .and_then(format_phone_jid)
            .or_else(|| format_phone_jid(&self.jid))
            .unwrap_or_else(|| "Unknown contact".to_owned())
    }
    pub fn same_person(&self, other: &Self) -> bool {
        let ids = |p: &Self| {
            [
                Some(p.jid.as_str()),
                p.phone_jid.as_deref(),
                p.lid.as_deref(),
                p.canonical_jid.as_deref(),
            ]
            .into_iter()
            .flatten()
            .filter_map(|s| s.parse::<Jid>().ok())
            .filter(|j| {
                (j.is_pn() || j.is_lid())
                    && !j.user_base().is_empty()
                    && j.user_base().bytes().all(|c| c.is_ascii_digit())
            })
            .map(|j| j.to_non_ad_string())
            .collect::<Vec<_>>()
        };
        let a = ids(self);
        let b = ids(other);
        a.iter().any(|id| b.contains(id))
    }
}

/// Only PN addresses carry a telephone number. No region is inferred.
pub fn format_phone_jid(raw: &str) -> Option<String> {
    let jid = raw.parse::<Jid>().ok()?.to_non_ad();
    let digits = jid.user_base();
    if !jid.server.is_pn_family()
        || !(7..=15).contains(&digits.len())
        || digits.starts_with('0')
        || !digits.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let international = format!("+{digits}");
    Some(
        phonenumber::parse(None, &international)
            .map(|p| {
                p.format()
                    .mode(phonenumber::Mode::International)
                    .to_string()
            })
            .unwrap_or(international),
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMembership {
    pub action: MembershipAction,
    pub actor: Option<NoticeParticipant>,
    pub participants: Vec<NoticeParticipant>,
}
impl GroupMembership {
    pub fn from_update(update: &GroupUpdate) -> Option<Self> {
        use GroupNotificationAction as A;
        let (action, participants) = match &*update.action {
            A::Add { participants, .. } => (MembershipAction::Add, participants),
            A::Remove { participants, .. } => (MembershipAction::Remove, participants),
            A::Promote { participants, .. } => (MembershipAction::Promote, participants),
            A::Demote { participants, .. } => (MembershipAction::Demote, participants),
            _ => return None,
        };
        Some(Self {
            action,
            actor: NoticeParticipant::actor_of(update),
            participants: participants
                .iter()
                .map(|p| NoticeParticipant {
                    jid: p.jid.to_string(),
                    phone_jid: p.phone_number.as_ref().map(ToString::to_string),
                    lid: p.lid.as_ref().map(ToString::to_string),
                    offered_name: p.display_name.clone(),
                    label: None,
                    is_self: false,
                    canonical_jid: None,
                })
                .collect(),
        })
    }
    pub fn summary(&self) -> String {
        let who = self
            .actor
            .as_ref()
            .map(|a| {
                if a.is_self {
                    "You".to_owned()
                } else {
                    a.label()
                }
            })
            .unwrap_or_else(|| "Someone".to_owned());
        // A leave notice names only the actor. Resolve identity before labels
        // so an unused participant cannot initialize phone metadata.
        if self.action == MembershipAction::Remove
            && self.participants.len() == 1
            && self
                .actor
                .as_ref()
                .is_some_and(|a| a.same_person(&self.participants[0]))
        {
            return format!("{who} left");
        }
        let names = match self.participants.as_slice() {
            [] => "someone".to_owned(),
            [one] => one.label(),
            [a, b] => format!("{} and {}", a.label(), b.label()),
            [one, rest @ ..] => format!("{} and {} others", one.label(), rest.len()),
        };
        match self.action {
            MembershipAction::Add => format!("{who} added {names}"),
            MembershipAction::Remove => format!("{who} removed {names}"),
            MembershipAction::Promote => format!(
                "{who} made {names} {}",
                if self.participants.len() > 1 {
                    "admins"
                } else {
                    "an admin"
                }
            ),
            MembershipAction::Demote => format!(
                "{who} removed {names} as {}",
                if self.participants.len() > 1 {
                    "admins"
                } else {
                    "admin"
                }
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn person(jid: &str, name: Option<&str>) -> NoticeParticipant {
        NoticeParticipant {
            jid: jid.to_owned(),
            phone_jid: None,
            lid: None,
            offered_name: name.map(str::to_owned),
            label: None,
            is_self: false,
            canonical_jid: None,
        }
    }
    fn event(action: MembershipAction, participants: Vec<NoticeParticipant>) -> GroupMembership {
        GroupMembership {
            action,
            actor: Some(person("559900000001@s.whatsapp.net", Some("Ana"))),
            participants,
        }
    }
    #[test]
    fn phone_fallback_never_formats_lids_or_device_ids_as_numbers() {
        assert_eq!(
            person("12025550123:7@s.whatsapp.net", None).label(),
            "+1 202-555-0123"
        );
        for raw in [
            "111000011112222@lid",
            "111000011112222:7@lid",
            "@s.whatsapp.net",
            "0@s.whatsapp.net",
            "abc@s.whatsapp.net",
            "",
            "broken",
        ] {
            assert_eq!(person(raw, None).label(), "Unknown contact", "{raw}");
        }
        let mut alias = person("111000011112222@lid", None);
        alias.phone_jid = Some("12025550123@s.whatsapp.net".to_owned());
        assert_eq!(alias.label(), "+1 202-555-0123");
    }
    #[test]
    fn phone_metadata_patch_preserves_labels_and_error_fallback() {
        // NANPA and Ofcom reserved fictional numbers. Device suffixes never
        // participate in international formatting.
        for (jid, expected) in [
            ("12025550123:7@s.whatsapp.net", "+1 202-555-0123"),
            ("442079460000@s.whatsapp.net", "+44 20 7946 0000"),
            ("447700900123@s.whatsapp.net", "+44 7700 900123"),
            ("9990000000@s.whatsapp.net", "+9990000000"),
        ] {
            assert_eq!(format_phone_jid(jid).as_deref(), Some(expected));
        }
        let mut contact = person("12025550123@s.whatsapp.net", Some("Alex"));
        assert_eq!(contact.label(), "Alex");
        contact.label = Some("Cached name".into());
        assert_eq!(contact.label(), "Cached name");

        // Only the immutable embedded database can skip runtime validation.
        let invalid = phonenumber::metadata::loader::Metadata {
            general: Some(phonenumber::metadata::loader::Descriptor {
                national_number: Some("[".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(phonenumber::metadata::Database::from(vec![invalid]).is_err());
    }

    #[test]
    fn membership_grammar_and_identities_survive_serialization() {
        let a = person("559900000002@s.whatsapp.net", Some("Ana"));
        let b = person("111000033334444@lid", Some("Bruno"));
        for (action, verb) in [
            (MembershipAction::Add, "added"),
            (MembershipAction::Remove, "removed"),
            (MembershipAction::Promote, "made"),
            (MembershipAction::Demote, "removed"),
        ] {
            for people in [
                vec![a.clone()],
                vec![a.clone(), b.clone()],
                vec![a.clone(), b.clone(), a.clone()],
            ] {
                let original = event(action, people);
                let restored: GroupMembership =
                    serde_json::from_str(&serde_json::to_string(&original).unwrap()).unwrap();
                assert_eq!(restored, original);
                assert!(restored.summary().starts_with(&format!("Ana {verb} Ana")));
                assert!(
                    !restored.summary().ends_with("left"),
                    "names do not establish identity"
                );
            }
        }
    }
    #[test]
    fn leaving_uses_normalized_identity_and_authoritative_aliases() {
        let mut target = person("111000011112222@lid", None);
        target.phone_jid = Some("559900000001:4@s.whatsapp.net".to_owned());
        let mut left = event(MembershipAction::Remove, vec![target]);
        assert_eq!(left.summary(), "Ana left");
        left.actor.as_mut().unwrap().is_self = true;
        assert_eq!(left.summary(), "You left");
        left.action = MembershipAction::Add;
        left.participants[0].is_self = true;
        assert_eq!(left.summary(), "You added you");
        assert_eq!(
            event(MembershipAction::Add, vec![]).summary(),
            "Ana added someone"
        );
    }
}
