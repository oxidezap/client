//! The timeline as one frame will draw it.
//!
//! Structure only: which rows exist and in what order. Nothing here knows how
//! tall anything is, and that is the point — the rows are measured as they
//! are laid out (see `components/message_list`), so a padding changed in a
//! bubble cannot disagree with a number kept over here.
//!
//! This file used to predict every row's height so a virtual list could size
//! it in advance, which made it and `message_bubble` two halves of one
//! contract kept in step by hand. Every drift showed up as bubbles overlapping
//! or floating apart, and the wrapping guess — characters per line, with no
//! idea of the font or the width — could never have been right.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::utils::crosses_day;
use oxidezap_core::{ChatMessage, TypingSummary};

/// One row of the timeline.
///
/// Date dividers and the typing indicator are list items rather than
/// decorations pinned around it: the list is virtual, so anything that takes
/// vertical space has to be something it can measure and scroll.
#[derive(Clone)]
pub enum TimelineItem {
    /// A heading over the messages of one day.
    DateDivider(DateTime<Utc>),
    Message {
        /// Index into [`MessageListCache::messages`].
        ix: usize,
        /// Whether this message starts a new run by the same author, which
        /// decides the sender name and the gap above it.
        starts_run: bool,
    },
    /// Always last, when someone is typing.
    Typing(TypingSummary),
    /// The standing notice at the head of a conversation. Not a stored
    /// message: it describes the chat rather than something that happened in
    /// it, so it is a row rather than a fabricated history entry.
    Encryption,
}

/// The timeline's rows, as one frame will draw them.
///
/// Cheap to rebuild — it walks the messages once — but rebuilt only when
/// something structural changed, because the identity of this value is what
/// tells the list whether its measurements still apply.
#[derive(Clone)]
pub struct MessageListCache {
    /// Which build of the rows this is.
    ///
    /// The virtual list caches a measured height per row index, and a row can
    /// change height without the count moving at all — an image arrives, a
    /// reaction lands, a message is revoked or a send fails and grows a retry
    /// button. Every one of those goes through the invalidation that rebuilds
    /// this, so a build number that differs from the one the list measured is
    /// exactly the signal that its measurements have stopped describing the
    /// rows.
    pub build: usize,
    /// Message count when the rows were built (invalidation check).
    pub message_count: usize,
    /// Group flag, which decides sender names and therefore run breaks.
    pub is_group: bool,
    /// The typing row that was included, so its arrival — or a change of who
    /// is typing — rebuilds the rows.
    pub typing: Option<TypingSummary>,
    /// The rows, dividers and typing indicator included.
    pub items: Arc<[TimelineItem]>,
    /// The messages the rows index into.
    pub messages: Arc<[ChatMessage]>,
}

/// What names one row, so it can be recognised at an index later.
///
/// Enough to say "this is still that row", which is all the timeline anchor
/// asks. Not enough to say the row is unchanged — a bubble grows a reaction
/// or a retry button without becoming a different row, and the build number
/// is what answers that.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RowId {
    Divider(DateTime<Utc>),
    Message(String),
    Typing,
    Encryption,
}

impl MessageListCache {
    /// Which row sits at `ix`, if anything does.
    ///
    /// The anchor asks this about one index — the last row whose measurement
    /// the list is keeping — because a prepend, a middle insertion and a
    /// removal all move that row and an append does not.
    pub fn row_id(&self, ix: usize) -> Option<RowId> {
        Some(match self.items.get(ix)? {
            TimelineItem::DateDivider(day) => RowId::Divider(*day),
            TimelineItem::Message { ix, .. } => RowId::Message(self.messages.get(*ix)?.id.clone()),
            TimelineItem::Typing(_) => RowId::Typing,
            TimelineItem::Encryption => RowId::Encryption,
        })
    }
}

impl MessageListCache {
    /// Build the timeline for `messages`.
    pub fn new(messages: &[ChatMessage], is_group: bool, typing: Option<TypingSummary>) -> Self {
        static BUILDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        Self {
            build: BUILDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            message_count: messages.len(),
            is_group,
            items: build_items(messages, typing.clone()).into(),
            typing,
            messages: Arc::from(messages),
        }
    }

    /// Whether this snapshot still describes the current inputs.
    pub fn is_valid_for(
        &self,
        message_count: usize,
        is_group: bool,
        typing: Option<&TypingSummary>,
    ) -> bool {
        self.message_count == message_count
            && self.is_group == is_group
            && self.typing.as_ref() == typing
    }
}

/// Weave dividers and the typing row into the message sequence.
fn build_items(messages: &[ChatMessage], typing: Option<TypingSummary>) -> Vec<TimelineItem> {
    // One divider per day plus one row per message, plus the typing row.
    let mut items = Vec::with_capacity(messages.len() + 5);
    // Only over real history: on an empty chat the pane shows an empty state,
    // and a lone encryption notice above nothing reads as a broken screen.
    if !messages.is_empty() {
        items.push(TimelineItem::Encryption);
    }

    for (ix, message) in messages.iter().enumerate() {
        let previous = ix.checked_sub(1).map(|prev| &messages[prev]);
        // The first message of the conversation always gets a divider: it
        // dates the start of what is loaded, which is the one place a reader
        // needs it most.
        let needs_divider = previous
            .map(|prev| crosses_day(&prev.timestamp, &message.timestamp))
            .unwrap_or(true);
        if needs_divider {
            items.push(TimelineItem::DateDivider(message.timestamp));
        }

        // A run is broken by a different author, a change of side, or a
        // divider — after a date heading the next message reads as a first.
        let starts_run = needs_divider
            || previous
                .map(|prev| prev.sender != message.sender || prev.is_from_me != message.is_from_me)
                .unwrap_or(true);

        items.push(TimelineItem::Message { ix, starts_run });
    }

    if let Some(summary) = typing {
        items.push(TimelineItem::Typing(summary));
    }
    items
}

/// Check if this message should show the sender name (for grouping)
pub fn should_show_sender(messages: &[ChatMessage], index: usize) -> bool {
    if index == 0 {
        return true;
    }
    let current = &messages[index];
    let previous = &messages[index - 1];
    current.sender != previous.sender || current.is_from_me != previous.is_from_me
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use oxidezap_core::Typist;

    fn at(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, day, hour, 0, 0).unwrap()
    }

    fn message(sender: &str, from_me: bool, when: DateTime<Utc>) -> ChatMessage {
        let mut msg = ChatMessage::new_incoming(
            format!("{sender}-{}", when.timestamp()),
            sender.to_string(),
            "hi".to_string(),
        );
        msg.is_from_me = from_me;
        msg.timestamp = when;
        msg
    }

    fn kinds(items: &[TimelineItem]) -> Vec<&'static str> {
        items
            .iter()
            .map(|item| match item {
                TimelineItem::DateDivider(_) => "divider",
                TimelineItem::Message { .. } => "message",
                TimelineItem::Typing(_) => "typing",
                TimelineItem::Encryption => "encryption",
            })
            .collect()
    }

    /// What the timeline anchor compares: the row at the end of the prefix
    /// the list has measured. An append leaves it where it was; a backfill
    /// before the head, and a row inserted in the middle, both push it along
    /// — and both raise the count exactly as an arrival does.
    #[test]
    fn the_last_measured_row_is_what_says_the_rest_did_not_move() {
        let first = message("a", false, at(13, 9));
        let last = message("a", false, at(14, 9));

        let before = MessageListCache::new(&[first.clone(), last.clone()], false, None);
        let boundary = before.row_id(before.items.len() - 1);
        assert_eq!(boundary, Some(RowId::Message(last.id.clone())));

        let appended = MessageListCache::new(
            &[first.clone(), last.clone(), message("a", false, at(15, 9))],
            false,
            None,
        );
        assert_eq!(
            appended.row_id(before.items.len() - 1),
            boundary,
            "an append leaves every earlier row where it was"
        );

        // Inserted between the two, the way a system notice stamped in the
        // past joins a conversation.
        let inserted =
            MessageListCache::new(&[first, message("a", false, at(13, 18)), last], false, None);
        assert_ne!(
            inserted.row_id(before.items.len() - 1),
            boundary,
            "a row in the middle moves the one the list measured last"
        );
    }

    /// A rebuild is the signal that something inside a row changed, and it is
    /// the only one when the count did not move.
    #[test]
    fn every_build_of_the_rows_is_its_own() {
        let messages = [message("a", false, at(14, 9))];
        let first = MessageListCache::new(&messages, false, None);
        let second = MessageListCache::new(&messages, false, None);
        assert_ne!(first.build, second.build);
        assert_eq!(first.message_count, second.message_count);
    }

    #[test]
    fn the_first_message_is_always_dated() {
        let items = build_items(&[message("a", false, at(14, 9))], None);
        assert_eq!(kinds(&items), vec!["encryption", "divider", "message"]);
    }

    #[test]
    fn a_divider_appears_only_where_the_day_changes() {
        let messages = vec![
            message("a", false, at(14, 9)),
            message("a", false, at(14, 18)),
            message("a", false, at(15, 8)),
        ];
        assert_eq!(
            kinds(&build_items(&messages, None)),
            vec![
                "encryption",
                "divider",
                "message",
                "message",
                "divider",
                "message"
            ]
        );
    }

    #[test]
    fn a_run_breaks_when_the_author_changes() {
        let messages = vec![
            message("a", false, at(14, 9)),
            message("a", false, at(14, 10)),
            message("b", false, at(14, 11)),
        ];
        let items = build_items(&messages, None);
        let runs: Vec<bool> = items
            .iter()
            .filter_map(|item| match item {
                TimelineItem::Message { starts_run, .. } => Some(*starts_run),
                _ => None,
            })
            .collect();
        assert_eq!(runs, vec![true, false, true]);
    }

    #[test]
    fn a_run_breaks_across_a_date_divider() {
        // Otherwise the first message of a new day loses its sender name and
        // reads as a continuation of yesterday.
        let messages = vec![
            message("a", false, at(14, 23)),
            message("a", false, at(15, 8)),
        ];
        let items = build_items(&messages, None);
        let TimelineItem::Message { starts_run, .. } = items[4] else {
            panic!("expected a message after the second divider");
        };
        assert!(starts_run);
    }

    #[test]
    fn a_run_breaks_when_the_side_changes() {
        let messages = vec![
            message("a", false, at(14, 9)),
            message("a", true, at(14, 10)),
        ];
        let items = build_items(&messages, None);
        let TimelineItem::Message { starts_run, .. } = items[3] else {
            panic!("expected a second message");
        };
        assert!(starts_run, "our own reply is not a continuation of theirs");
    }

    #[test]
    fn typing_is_the_last_row() {
        let summary = TypingSummary {
            typists: vec![Typist {
                jid: "a@s.whatsapp.net".to_string(),
                name: "Ana".to_string(),
            }],
            total: 1,
            kind: oxidezap_core::ComposingKind::Text,
        };
        let items = build_items(&[message("a", false, at(14, 9))], Some(summary));
        assert_eq!(
            kinds(&items),
            vec!["encryption", "divider", "message", "typing"]
        );
    }

    #[test]
    fn the_rows_survive_a_reason_to_rebuild() {
        let messages = vec![message("a", false, at(14, 9))];
        let cache = MessageListCache::new(&messages, false, None);
        assert!(cache.is_valid_for(1, false, None));
        assert!(
            !cache.is_valid_for(2, false, None),
            "a new message is a new row"
        );
        assert!(
            !cache.is_valid_for(1, true, None),
            "grouping decides sender names, so it decides run breaks"
        );
    }

    #[test]
    fn typing_arriving_rebuilds_the_rows() {
        let messages = vec![message("a", false, at(14, 9))];
        let cache = MessageListCache::new(&messages, false, None);
        assert!(!cache.is_valid_for(1, false, Some(&typing(&["Ana"]))));
    }

    /// A second typist in a group adds an avatar to the row, so a summary
    /// that changed while staying `Some` is a different timeline.
    #[test]
    fn a_changed_typing_summary_rebuilds_the_rows() {
        let messages = vec![message("a", false, at(14, 9))];
        let cache = MessageListCache::new(&messages, true, Some(typing(&["Ana"])));
        assert!(cache.is_valid_for(1, true, Some(&typing(&["Ana"]))));
        assert!(!cache.is_valid_for(1, true, Some(&typing(&["Ana", "Marcos"]))));
    }

    fn typing(names: &[&str]) -> TypingSummary {
        TypingSummary {
            typists: names
                .iter()
                .map(|n| Typist {
                    jid: format!("{n}@s.whatsapp.net"),
                    name: (*n).to_string(),
                })
                .collect(),
            total: names.len(),
            kind: oxidezap_core::ComposingKind::Text,
        }
    }
}
