//! Chat list state: filtering, and the per-frame row snapshot.

use std::sync::Arc;

use oxidezap_core::Chat;

use super::chat_row::ChatRow;

/// Which conversations the sidebar is showing.
///
/// A filter is part of the information model, not a view detail: the list
/// being short has to be explainable, so the active filter stays visible and
/// an empty result offers the way back to `All`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChatFilter {
    #[default]
    All,
    Unread,
    Groups,
}

impl ChatFilter {
    pub const ALL: [Self; 3] = [Self::All, Self::Unread, Self::Groups];

    pub fn id(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Unread => "unread",
            Self::Groups => "groups",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Unread => "Unread",
            Self::Groups => "Groups",
        }
    }

    /// Whether `chat` belongs under this filter.
    pub fn matches(self, chat: &Chat) -> bool {
        match self {
            Self::All => true,
            Self::Unread => chat.unread_count > 0 || chat.manually_unread,
            Self::Groups => chat.is_group,
        }
    }
}

/// The conversation list as one frame will draw it.
///
/// Rows are derived once and shared, rather than recomputed per visible item:
/// the virtual list rebuilds its range on every scroll, and working out a
/// preview per row per frame is exactly the kind of work that makes scrolling
/// feel heavy.
#[derive(Clone)]
pub struct ChatListCache {
    /// Chat count the snapshot was taken at, for invalidation.
    pub chat_count: usize,
    pub rows: Arc<[ChatRow]>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(jid: &str, is_group: bool, unread: u32, marked: bool) -> Chat {
        let mut chat = Chat::new(jid.to_string());
        chat.is_group = is_group;
        chat.unread_count = unread;
        chat.manually_unread = marked;
        chat
    }

    #[test]
    fn all_keeps_everything() {
        assert!(ChatFilter::All.matches(&chat("a@s.whatsapp.net", false, 0, false)));
        assert!(ChatFilter::All.matches(&chat("g@g.us", true, 0, false)));
    }

    #[test]
    fn unread_counts_both_kinds_of_unread() {
        assert!(ChatFilter::Unread.matches(&chat("a@s.whatsapp.net", false, 3, false)));
        assert!(
            ChatFilter::Unread.matches(&chat("a@s.whatsapp.net", false, 0, true)),
            "marked unread by hand is still unread"
        );
        assert!(!ChatFilter::Unread.matches(&chat("a@s.whatsapp.net", false, 0, false)));
    }

    #[test]
    fn groups_excludes_direct_chats() {
        assert!(ChatFilter::Groups.matches(&chat("g@g.us", true, 0, false)));
        assert!(!ChatFilter::Groups.matches(&chat("a@s.whatsapp.net", false, 9, false)));
    }

    #[test]
    fn filter_ids_are_stable_and_distinct() {
        let ids: Vec<&str> = ChatFilter::ALL.iter().map(|f| f.id()).collect();
        assert_eq!(ids, vec!["all", "unread", "groups"]);
    }
}
