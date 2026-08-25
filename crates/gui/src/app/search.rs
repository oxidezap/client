//! Searching inside one conversation.
//!
//! Distinct from the sidebar's field, which filters *chats* by name. The
//! header's magnifier said "Search in conversation" and focused that one, so
//! the control did something other than what it was labelled. This is the
//! thing it was labelled.
//!
//! It searches what is loaded, and says so when that is all it searched: the
//! store's FTS index lives behind the daemon, and a control that silently
//! covers only part of a history is worse than one that names its horizon.

use oxidezap_core::ChatMessage;

/// One conversation's search, while it is open.
pub struct ConversationSearch {
    /// The chat being searched. Switching chats closes the search rather than
    /// carrying a query into a conversation it was never typed for.
    pub jid: String,
    /// Lowercased and trimmed, like the chat-list query.
    pub query: String,
    /// Ids of the matching messages, newest last — timeline order, so
    /// "next" walks the way the eye does.
    pub matches: Vec<String>,
    /// Which match is current, when there are any.
    pub current: usize,
}

impl ConversationSearch {
    pub fn new(jid: String) -> Self {
        Self {
            jid,
            query: String::new(),
            matches: Vec::new(),
            current: 0,
        }
    }

    /// The id the timeline should be showing.
    pub fn current_match(&self) -> Option<&str> {
        self.matches.get(self.current).map(String::as_str)
    }

    /// "3 of 12", or what to say instead.
    ///
    /// The empty answer names its horizon. A conversation holds the messages
    /// this window has loaded — one page, unless the reader has asked for
    /// more — while the store behind the daemon holds the rest and an FTS
    /// index over it. "No matches" is a claim about the whole history that
    /// this search is in no position to make; "in the loaded messages" is
    /// what it actually looked at.
    pub fn status(&self) -> Option<String> {
        if self.query.is_empty() {
            return None;
        }
        if self.matches.is_empty() {
            return Some("No matches in the loaded messages".to_string());
        }
        Some(format!("{} of {}", self.current + 1, self.matches.len()))
    }

    pub fn has_matches(&self) -> bool {
        !self.matches.is_empty()
    }

    /// Re-run the query over `messages`.
    ///
    /// Keeps the reader where they were when it can: narrowing a query
    /// usually leaves the current hit in the result, and jumping them back to
    /// the top of the conversation for a typed character is disorienting.
    pub fn refresh(&mut self, query: &str, messages: &[ChatMessage]) {
        let previous = self.current_match().map(str::to_string);
        self.query = query.trim().to_lowercase();
        self.matches = if self.query.is_empty() {
            Vec::new()
        } else {
            messages
                .iter()
                .filter(|message| matches(message, &self.query))
                .map(|message| message.id.clone())
                .collect()
        };
        self.current = previous
            .and_then(|id| self.matches.iter().position(|candidate| *candidate == id))
            // Otherwise the newest match, which is the one nearest where the
            // reader already is.
            .unwrap_or(self.matches.len().saturating_sub(1));
    }

    /// Step to the next match, wrapping. Returns the id to jump to.
    pub fn step(&mut self, forward: bool) -> Option<&str> {
        if self.matches.is_empty() {
            return None;
        }
        let last = self.matches.len() - 1;
        self.current = if forward {
            if self.current >= last {
                0
            } else {
                self.current + 1
            }
        } else if self.current == 0 {
            last
        } else {
            self.current - 1
        };
        self.current_match()
    }
}

/// Whether a message answers `query`, which is already lowercased.
///
/// Captions and file names count: a photo is often remembered by what was
/// said about it, and a document by what it was called.
fn matches(message: &ChatMessage, query: &str) -> bool {
    if message.content.to_lowercase().contains(query) {
        return true;
    }
    message.media.as_ref().is_some_and(|media| {
        media
            .file_name
            .as_ref()
            .is_some_and(|name| name.to_lowercase().contains(query))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn message(id: &str, content: &str) -> ChatMessage {
        let mut message = ChatMessage::new_incoming(
            id.to_string(),
            "a@s.whatsapp.net".to_string(),
            content.to_string(),
        );
        message.timestamp = DateTime::<Utc>::from_timestamp(0, 0).expect("epoch");
        message
    }

    fn history() -> Vec<ChatMessage> {
        vec![
            message("1", "the invoice is attached"),
            message("2", "thanks"),
            message("3", "Invoice again, sorry"),
        ]
    }

    #[test]
    fn matching_ignores_case_and_keeps_timeline_order() {
        let mut search = ConversationSearch::new("chat".into());
        search.refresh("INVOICE", &history());
        assert_eq!(search.matches, vec!["1".to_string(), "3".to_string()]);
    }

    #[test]
    fn the_newest_match_is_the_one_shown_first() {
        let mut search = ConversationSearch::new("chat".into());
        search.refresh("invoice", &history());
        assert_eq!(search.current_match(), Some("3"));
        assert_eq!(search.status().as_deref(), Some("2 of 2"));
    }

    #[test]
    fn stepping_wraps_in_both_directions() {
        let mut search = ConversationSearch::new("chat".into());
        search.refresh("invoice", &history());
        assert_eq!(search.step(true), Some("1"), "forward from the last wraps");
        assert_eq!(search.step(false), Some("3"), "and back again");
    }

    /// Typing one more character must not throw the reader back to the top.
    #[test]
    fn narrowing_a_query_keeps_the_current_hit() {
        let history = history();
        let mut search = ConversationSearch::new("chat".into());
        search.refresh("invoice", &history);
        search.step(true);
        assert_eq!(search.current_match(), Some("1"));

        search.refresh("invoice is", &history);
        assert_eq!(search.current_match(), Some("1"));
    }

    #[test]
    fn an_empty_query_says_nothing_rather_than_no_matches() {
        let mut search = ConversationSearch::new("chat".into());
        search.refresh("   ", &history());
        assert!(search.status().is_none());
        assert!(!search.has_matches());
    }

    #[test]
    fn a_query_nothing_answers_says_so() {
        let mut search = ConversationSearch::new("chat".into());
        search.refresh("receipt", &history());
        assert_eq!(
            search.status().as_deref(),
            Some("No matches in the loaded messages")
        );
    }
}
