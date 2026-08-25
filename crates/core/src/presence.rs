//! Who is typing, and who is around.
//!
//! Presence is the one part of chat state that expires on its own. A
//! `composing` notice is a claim about *right now*, and the matching `paused`
//! is not guaranteed to arrive — the peer can lose its connection mid-word.
//! Every entry therefore carries its own deadline and the registry is read
//! through it, so a stale "typing…" cannot outlive the typing.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// How many names a summary spells out before collapsing the rest into a
/// count. Three is where the line stops fitting a sidebar row.
const MAX_NAMED_TYPISTS: usize = 2;

/// How long a `composing` notice is trusted without renewal.
///
/// WhatsApp's own clients re-send while the user keeps typing, so this only
/// has to outlive the gap between keystrokes — not a whole message.
const COMPOSING_TTL_SECS: i64 = 10;

/// What the peer is composing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposingKind {
    #[default]
    Text,
    /// Holding the record button. Worth distinguishing: it tells the reader a
    /// voice note is coming, not a line of text.
    Audio,
}

impl ComposingKind {
    /// The verb for a single participant.
    pub fn verb(self) -> &'static str {
        match self {
            Self::Text => "typing",
            Self::Audio => "recording audio",
        }
    }
}

#[derive(Debug, Clone)]
struct Composing {
    name: String,
    kind: ComposingKind,
    expires_at: DateTime<Utc>,
}

/// Who is typing in one conversation.
#[derive(Debug, Clone, Default)]
pub struct ChatTyping {
    /// Keyed by sender JID: a second notice from the same person renews their
    /// deadline rather than listing them twice.
    participants: HashMap<String, Composing>,
}

impl ChatTyping {
    fn set(&mut self, sender: String, name: String, kind: ComposingKind, now: DateTime<Utc>) {
        self.participants.insert(
            sender,
            Composing {
                name,
                kind,
                expires_at: now + Duration::seconds(COMPOSING_TTL_SECS),
            },
        );
    }

    fn clear(&mut self, sender: &str) {
        self.participants.remove(sender);
    }

    fn drop_expired(&mut self, now: DateTime<Utc>) {
        self.participants.retain(|_, c| c.expires_at > now);
    }

    fn is_empty(&self) -> bool {
        self.participants.is_empty()
    }

    /// Who is currently typing, in a stable order.
    ///
    /// Sorted by name so a re-render cannot reshuffle the avatars while the
    /// same people keep typing.
    fn summary(&self, now: DateTime<Utc>) -> Option<TypingSummary> {
        let mut live: Vec<&Composing> = self
            .participants
            .values()
            .filter(|c| c.expires_at > now)
            .collect();
        if live.is_empty() {
            return None;
        }
        live.sort_by(|a, b| a.name.cmp(&b.name));

        Some(TypingSummary {
            // One person recording makes it a recording; a crowd is just
            // "typing", because the mixed case has no honest short phrasing.
            kind: if live.len() == 1 {
                live[0].kind
            } else {
                ComposingKind::Text
            },
            names: live
                .iter()
                .take(MAX_NAMED_TYPISTS)
                .map(|c| c.name.clone())
                .collect(),
            total: live.len(),
        })
    }
}

/// A rendered-ready view of who is typing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypingSummary {
    /// Up to [`MAX_NAMED_TYPISTS`] names, alphabetical.
    pub names: Vec<String>,
    /// How many are typing in total, including the unnamed remainder.
    pub total: usize,
    pub kind: ComposingKind,
}

impl TypingSummary {
    /// How many typists are not spelled out by name.
    pub fn overflow(&self) -> usize {
        self.total.saturating_sub(self.names.len())
    }

    /// The sentence shown beside the avatars in a group: `Ana is typing`,
    /// `Ana, Marcos +2 are typing`.
    ///
    /// A 1:1 chat does not use this — there is only one person it could be, so
    /// the bubble alone says it.
    pub fn label(&self) -> String {
        let mut names = self.names.join(", ");
        if self.overflow() > 0 {
            names.push_str(&format!(" +{}", self.overflow()));
        }
        let verb = if self.total == 1 {
            format!("is {}", self.kind.verb())
        } else {
            format!("are {}", self.kind.verb())
        };
        format!("{names} {verb}")
    }

    /// The compact form for a chat header or a list row, where the name is
    /// already the row's subject: `typing…`, `Ana, Marcos +2 typing…`.
    pub fn compact_label(&self, is_group: bool) -> String {
        if !is_group {
            return format!("{}…", self.kind.verb());
        }
        let mut names = self.names.join(", ");
        if self.overflow() > 0 {
            names.push_str(&format!(" +{}", self.overflow()));
        }
        format!("{names} {}…", self.kind.verb())
    }
}

/// Where a contact is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Online,
    /// Last seen at this moment, when the contact shares it.
    LastSeen(DateTime<Utc>),
    /// Offline, and not sharing when they were last around.
    Unknown,
}

/// All presence the UI knows about, for every conversation.
///
/// Owned by the front end rather than persisted: it describes this moment and
/// is worthless a restart later.
#[derive(Debug, Default)]
pub struct PresenceRegistry {
    typing: HashMap<String, ChatTyping>,
    availability: HashMap<String, Availability>,
}

impl PresenceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `sender` started composing in `chat`.
    pub fn set_composing(
        &mut self,
        chat: String,
        sender: String,
        name: String,
        kind: ComposingKind,
    ) {
        self.typing
            .entry(chat)
            .or_default()
            .set(sender, name, kind, wacore::time::now_utc());
    }

    /// Record that `sender` stopped.
    pub fn clear_composing(&mut self, chat: &str, sender: &str) {
        if let Some(entry) = self.typing.get_mut(chat) {
            entry.clear(sender);
            if entry.is_empty() {
                self.typing.remove(chat);
            }
        }
    }

    /// Forget everything about a conversation — used when a message from that
    /// sender arrives, which ends their composing more reliably than `paused`.
    pub fn clear_chat(&mut self, chat: &str) {
        self.typing.remove(chat);
    }

    pub fn set_availability(&mut self, jid: String, availability: Availability) {
        self.availability.insert(jid, availability);
    }

    pub fn availability(&self, jid: &str) -> Option<&Availability> {
        self.availability.get(jid)
    }

    /// Who is typing in `chat` right now, or `None`.
    pub fn typing(&self, chat: &str) -> Option<TypingSummary> {
        self.typing
            .get(chat)
            .and_then(|entry| entry.summary(wacore::time::now_utc()))
    }

    /// Whether anyone at all is typing, so a render pass can skip the work.
    pub fn has_typing(&self) -> bool {
        let now = wacore::time::now_utc();
        self.typing
            .values()
            .any(|entry| entry.summary(now).is_some())
    }

    /// Drop entries whose deadline has passed.
    ///
    /// Reads are already filtered by deadline, so this is only housekeeping:
    /// it keeps the map from growing with people who typed once and left.
    /// Returns whether anything was removed, so the caller can skip a redraw.
    pub fn prune(&mut self) -> bool {
        let now = wacore::time::now_utc();
        let before = self.typing.len();
        for entry in self.typing.values_mut() {
            entry.drop_expired(now);
        }
        self.typing.retain(|_, entry| !entry.is_empty());
        before != self.typing.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAT: &str = "group@g.us";

    fn registry_with(names: &[(&str, &str)]) -> PresenceRegistry {
        let mut registry = PresenceRegistry::new();
        for (jid, name) in names {
            registry.set_composing(
                CHAT.to_string(),
                (*jid).to_string(),
                (*name).to_string(),
                ComposingKind::Text,
            );
        }
        registry
    }

    #[test]
    fn one_typist_is_named_in_the_singular() {
        let registry = registry_with(&[("a@s.whatsapp.net", "Ana")]);
        let summary = registry.typing(CHAT).expect("someone is typing");
        assert_eq!(summary.label(), "Ana is typing");
        assert_eq!(summary.total, 1);
        assert_eq!(summary.overflow(), 0);
    }

    #[test]
    fn a_crowd_names_two_and_counts_the_rest() {
        let registry = registry_with(&[
            ("a@s.whatsapp.net", "Ana"),
            ("m@s.whatsapp.net", "Marcos"),
            ("p@s.whatsapp.net", "Paula"),
            ("r@s.whatsapp.net", "Rui"),
        ]);
        let summary = registry.typing(CHAT).unwrap();
        assert_eq!(summary.label(), "Ana, Marcos +2 are typing");
        assert_eq!(summary.overflow(), 2);
    }

    #[test]
    fn repeated_notices_renew_rather_than_duplicate() {
        let mut registry = registry_with(&[("a@s.whatsapp.net", "Ana")]);
        registry.set_composing(
            CHAT.to_string(),
            "a@s.whatsapp.net".to_string(),
            "Ana".to_string(),
            ComposingKind::Text,
        );
        assert_eq!(registry.typing(CHAT).unwrap().total, 1);
    }

    #[test]
    fn recording_audio_reads_differently_from_typing() {
        let mut registry = PresenceRegistry::new();
        registry.set_composing(
            CHAT.to_string(),
            "a@s.whatsapp.net".to_string(),
            "Ana".to_string(),
            ComposingKind::Audio,
        );
        let summary = registry.typing(CHAT).unwrap();
        assert_eq!(summary.label(), "Ana is recording audio");
        assert_eq!(summary.compact_label(false), "recording audio…");
    }

    #[test]
    fn a_direct_chat_does_not_repeat_the_only_possible_name() {
        let registry = registry_with(&[("a@s.whatsapp.net", "Ana")]);
        let summary = registry.typing(CHAT).unwrap();
        assert_eq!(summary.compact_label(false), "typing…");
        assert_eq!(summary.compact_label(true), "Ana typing…");
    }

    #[test]
    fn pausing_removes_the_typist_and_then_the_chat() {
        let mut registry = registry_with(&[("a@s.whatsapp.net", "Ana")]);
        registry.clear_composing(CHAT, "a@s.whatsapp.net");
        assert!(registry.typing(CHAT).is_none());
        assert!(!registry.has_typing());
    }

    #[test]
    fn an_arriving_message_ends_composing_for_the_whole_chat() {
        let mut registry = registry_with(&[("a@s.whatsapp.net", "Ana")]);
        registry.clear_chat(CHAT);
        assert!(registry.typing(CHAT).is_none());
    }

    #[test]
    fn names_are_ordered_so_avatars_do_not_reshuffle() {
        let registry = registry_with(&[
            ("z@s.whatsapp.net", "Zoe"),
            ("a@s.whatsapp.net", "Ana"),
            ("m@s.whatsapp.net", "Marcos"),
        ]);
        let first = registry.typing(CHAT).unwrap();
        let second = registry.typing(CHAT).unwrap();
        assert_eq!(first.names, vec!["Ana", "Marcos"]);
        assert_eq!(first, second);
    }

    #[test]
    fn availability_is_remembered_per_contact() {
        let mut registry = PresenceRegistry::new();
        registry.set_availability("a@s.whatsapp.net".to_string(), Availability::Online);
        assert_eq!(
            registry.availability("a@s.whatsapp.net"),
            Some(&Availability::Online)
        );
        assert_eq!(registry.availability("b@s.whatsapp.net"), None);
    }
}
