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

impl MessageListCache {
    /// How many rows at the front of these two are the same row, drawn the
    /// same way.
    ///
    /// The front is not a safe assumption on its own: the encryption notice
    /// sits at index 0 whatever else happens, and a page of older history
    /// lands *after* it — so a prepend is an insertion in the middle of the
    /// rows, not at the top of them. It can also swallow a divider, when the
    /// page's newest message shares a day with the oldest already drawn.
    pub fn common_prefix(&self, other: &Self) -> usize {
        (0..self.items.len().min(other.items.len()))
            .take_while(|ix| self.row_matches(*ix, other, *ix))
            .count()
    }

    /// The same from the end, over the rows neither side has already spent on
    /// [`Self::common_prefix`].
    pub fn common_suffix(&self, other: &Self, spent: usize) -> usize {
        let room = (self.items.len().min(other.items.len())).saturating_sub(spent);
        (1..=room)
            .take_while(|back| {
                self.row_matches(self.items.len() - back, other, other.items.len() - back)
            })
            .count()
    }

    /// Whether two rows are the same row *and* would be drawn the same way.
    ///
    /// Identity is not enough: a message that stops starting a run loses its
    /// name and avatar, which is a height the list would otherwise keep. The
    /// bytes inside a bubble are the `build` number's business; this is about
    /// what the row is.
    fn row_matches(&self, ix: usize, other: &Self, other_ix: usize) -> bool {
        match (self.items.get(ix), other.items.get(other_ix)) {
            (
                Some(TimelineItem::Message { ix, starts_run }),
                Some(TimelineItem::Message {
                    ix: other_ix,
                    starts_run: other_starts_run,
                }),
            ) => {
                starts_run == other_starts_run
                    && self.messages.get(*ix).map(|m| &m.id)
                        == other.messages.get(*other_ix).map(|m| &m.id)
            }
            (Some(TimelineItem::DateDivider(day)), Some(TimelineItem::DateDivider(other_day))) => {
                day == other_day
            }
            (Some(TimelineItem::Typing(summary)), Some(TimelineItem::Typing(other_summary))) => {
                summary == other_summary
            }
            (Some(TimelineItem::Encryption), Some(TimelineItem::Encryption)) => true,
            _ => false,
        }
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

    /// A conversation with no history can still have the other side typing,
    /// and that row is the only thing on screen with anything to say. The
    /// pane decided by the chat's messages rather than by the list's rows, so
    /// it drew "No messages yet" over a live indicator — with the list
    /// already synchronized to one row.
    #[test]
    fn a_new_chat_with_somebody_typing_has_a_row_to_draw() {
        let items = build_items(&[], Some(typing(&["Ana"])));
        assert_eq!(kinds(&items), vec!["typing"]);
        assert!(
            !items.is_empty(),
            "and the pane draws by this, not by the message count"
        );
    }

    /// What the timeline anchor compares: which rows the two frames have in
    /// common at either end, and therefore the stretch between them that
    /// changed. An append changes the end alone; a page of older history
    /// changes a stretch that starts *after* the encryption notice.
    #[test]
    fn what_two_frames_share_is_what_the_list_may_keep() {
        let first = message("a", false, at(13, 9));
        let last = message("a", false, at(14, 9));
        let before = MessageListCache::new(&[first.clone(), last.clone()], false, None);

        let appended = MessageListCache::new(
            &[first.clone(), last.clone(), message("a", false, at(15, 9))],
            false,
            None,
        );
        let at_ix = before.common_prefix(&appended);
        assert_eq!(
            at_ix,
            before.items.len(),
            "an append leaves every earlier row where it was"
        );
        assert_eq!(before.common_suffix(&appended, at_ix), 0);

        // A page of older history. The encryption notice is still at index 0,
        // which is exactly the row a splice at the front would trample.
        let paged = MessageListCache::new(
            &[message("a", false, at(11, 9)), first.clone(), last.clone()],
            false,
            None,
        );
        let at_ix = before.common_prefix(&paged);
        assert_eq!(at_ix, 1, "the encryption notice, and nothing after it");
        assert_eq!(
            before.common_suffix(&paged, at_ix),
            before.items.len() - 1,
            "everything else is still there, further down"
        );

        // Inserted between the two, the way a system notice stamped in the
        // past joins a conversation: neither end alone accounts for it.
        let inserted =
            MessageListCache::new(&[first, message("a", false, at(13, 18)), last], false, None);
        let at_ix = before.common_prefix(&inserted);
        assert!(at_ix > 0 && at_ix < before.items.len());
        assert!(before.common_suffix(&inserted, at_ix) < before.items.len());
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
