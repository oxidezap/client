//! The timeline as one frame will draw it.
//!
//! The virtual list has to size a row before it renders it, so every height
//! here is a prediction of what the bubble will lay out to. That makes this
//! file and `message_bubble` two halves of one contract: a padding changed in
//! one without the other shows up as overlapping or gapped rows.
//!
//! Heights are resolved geometry, so the cache keys on the metrics that
//! produced them — base font and density included.

use std::rc::Rc;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use gpui::{Pixels, Size, size};

use crate::theme::Metrics;
use crate::utils::{crosses_day, scale_media_dimensions};
use oxidezap_core::{ChatMessage, MediaType, TypingSummary};

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

/// Cached data for message list rendering to avoid recomputing on every frame.
#[derive(Clone)]
pub struct MessageListCache {
    /// Message count when cache was created (invalidation check)
    pub message_count: usize,
    /// Group flag the sizes were computed with (invalidation check)
    pub is_group: bool,
    /// Media size cap the sizes were computed with (invalidation check)
    pub max_media_size: f32,
    /// The scale the heights were measured against. A base-font or density
    /// change moves every row, and a stale measurement clips content.
    pub metrics: Metrics,
    /// Whether a typing row was included, so its arrival rebuilds the list.
    pub has_typing: bool,
    /// The rows, dividers and typing indicator included.
    pub items: Arc<[TimelineItem]>,
    /// Pre-computed item sizes for virtual list
    pub item_sizes: Rc<Vec<Size<Pixels>>>,
    /// Shared messages reference
    pub messages: Arc<[ChatMessage]>,
}

impl MessageListCache {
    /// Build the timeline for `messages`.
    pub fn new(
        messages: &[ChatMessage],
        is_group: bool,
        max_media_size: f32,
        metrics: Metrics,
        typing: Option<TypingSummary>,
    ) -> Self {
        let messages_arc: Arc<[ChatMessage]> = Arc::from(messages);
        let has_typing = typing.is_some();
        let items = build_items(messages, typing);

        let item_sizes: Rc<Vec<Size<Pixels>>> = Rc::new(
            items
                .iter()
                .map(|item| {
                    let height = match item {
                        TimelineItem::DateDivider(_) => metrics.date_divider_height(),
                        TimelineItem::Typing(_) => metrics.typing_row_height(),
                        TimelineItem::Encryption => metrics.typing_row_height(),
                        TimelineItem::Message { ix, starts_run } => calculate_message_height(
                            &messages[*ix],
                            *starts_run,
                            is_group,
                            max_media_size,
                            metrics,
                        ),
                    };
                    size(gpui::px(600.), height)
                })
                .collect(),
        );

        Self {
            message_count: messages.len(),
            is_group,
            max_media_size,
            metrics,
            has_typing,
            items: items.into(),
            item_sizes,
            messages: messages_arc,
        }
    }

    /// Whether this snapshot still describes the current inputs.
    pub fn is_valid_for(
        &self,
        message_count: usize,
        is_group: bool,
        max_media_size: f32,
        metrics: Metrics,
        has_typing: bool,
    ) -> bool {
        self.message_count == message_count
            && self.is_group == is_group
            && self.max_media_size == max_media_size
            && self.metrics == metrics
            && self.has_typing == has_typing
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

/// Predict the height of a message bubble.
///
/// `starts_run` must be the same flag the bubble renders with: it drives the
/// outer padding in every chat, while the sender-name line only exists in
/// groups.
pub fn calculate_message_height(
    msg: &ChatMessage,
    starts_run: bool,
    is_group: bool,
    max_media_size: f32,
    metrics: Metrics,
) -> Pixels {
    let gap = if starts_run {
        metrics.bubble_gap_authored()
    } else {
        metrics.bubble_gap_grouped()
    };
    // The time and ticks moved inside the bubble's last line, so there is no
    // longer a row of their own to budget for.
    let mut height = gap + metrics.bubble_padding_y() * 2.0;
    let mut content_items = 0;

    if is_group && starts_run && msg.sender_name.is_some() && !msg.is_from_me {
        height += metrics.line_height();
        content_items += 1;
    }

    // A system row is its own shape: one icon, two short lines, no bubble.
    if msg.system.is_some() {
        return gap + metrics.avatar_header() + metrics.space_md();
    }

    if msg.quoted.is_some() {
        // Name line plus preview line, inside the quote's own padding.
        height += metrics.line_height() * 2.0;
        content_items += 1;
    }

    if let Some(media) = &msg.media {
        height += match media.media_type {
            MediaType::Image | MediaType::Sticker | MediaType::Video => {
                let (_, h) = scale_media_dimensions(
                    media.width.unwrap_or(300),
                    media.height.unwrap_or(300),
                    max_media_size,
                );
                gpui::px(h)
            }
            // The voice player is a control row: play button, waveform, time.
            MediaType::Audio => metrics.waveform_height() + metrics.space_xl(),
            MediaType::Document => metrics.avatar_row() + metrics.space_md(),
        };
        content_items += 1;
    }

    if !msg.content.is_empty() {
        height += metrics.line_height() * wrapped_lines(&msg.content) as f32;
        content_items += 1;
    } else if msg.media.is_none() {
        // An empty bubble still has one line of chrome in it (the time), and
        // sizing it to nothing would overlap its neighbour.
        height += metrics.line_height();
    }

    if content_items > 1 {
        height += metrics.space_md() * (content_items - 1) as f32;
    }

    if !msg.reactions.is_empty() {
        // Reactions overlap the bubble's lower edge, so only the part that
        // hangs below it costs height.
        height += metrics.reaction_height() - metrics.reaction_overlap();
    }

    height
}

/// Roughly how many lines `content` will wrap to.
///
/// An estimate, deliberately: measuring shaped text for every message in a
/// conversation to size rows the reader may never scroll to is far more
/// expensive than being a line out on a long paragraph.
fn wrapped_lines(content: &str) -> usize {
    /// Characters per line at the design's bubble width and body size.
    const CHARS_PER_LINE: usize = 42;

    content
        .lines()
        .map(|line| line.chars().count().div_ceil(CHARS_PER_LINE).max(1))
        .sum::<usize>()
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

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
            names: vec!["Ana".to_string()],
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
    fn every_item_gets_a_size() {
        let messages = vec![
            message("a", false, at(14, 9)),
            message("b", false, at(15, 9)),
        ];
        let cache = MessageListCache::new(&messages, true, 300.0, Metrics::default(), None);
        assert_eq!(cache.item_sizes.len(), cache.items.len());
        assert!(cache.item_sizes.iter().all(|s| s.height > gpui::px(0.0)));
    }

    #[test]
    fn a_zoom_change_invalidates_the_measured_rows() {
        let messages = vec![message("a", false, at(14, 9))];
        let cache = MessageListCache::new(&messages, false, 300.0, Metrics::default(), None);
        let zoomed = Metrics::new(20.0, crate::theme::metrics::Density::Comfortable);
        assert!(
            !cache.is_valid_for(1, false, 300.0, zoomed, false),
            "rows measured at one base font cannot be reused at another"
        );
        assert!(cache.is_valid_for(1, false, 300.0, Metrics::default(), false));
    }

    #[test]
    fn typing_arriving_invalidates_the_list() {
        let messages = vec![message("a", false, at(14, 9))];
        let cache = MessageListCache::new(&messages, false, 300.0, Metrics::default(), None);
        assert!(!cache.is_valid_for(1, false, 300.0, Metrics::default(), true));
    }

    #[test]
    fn line_estimates_count_hard_breaks_and_wrapping() {
        assert_eq!(wrapped_lines(""), 1);
        assert_eq!(wrapped_lines("short"), 1);
        assert_eq!(wrapped_lines("a\nb\nc"), 3);
        assert_eq!(wrapped_lines(&"x".repeat(84)), 2);
        assert_eq!(wrapped_lines(&"x".repeat(85)), 3);
    }

    #[test]
    fn an_empty_bubble_still_reserves_a_line() {
        let mut msg = message("a", false, at(14, 9));
        msg.content.clear();
        let height = calculate_message_height(&msg, true, false, 300.0, Metrics::default());
        assert!(height > gpui::px(0.0));
    }
}
