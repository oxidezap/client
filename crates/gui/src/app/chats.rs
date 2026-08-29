//! Chat list state: filtering, and the per-frame row snapshot.

use std::sync::Arc;

use oxidezap_core::Chat;

use super::chat_row::ChatRow;
use super::{WhatsAppApp, newest_shared_message};
use log::info;
use wacore_binary::jid::observe_str;

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

/// What a complete store load says about a chat already on screen.
///
/// A complete load is the store's whole truth about the rows it has, so a
/// store-backed chat missing from one was archived or deleted — possibly on
/// another device — and has to leave the window too. Two things stop that
/// being a plain removal, and naming them here is what keeps the rule in one
/// place: a live-only chat was never in the store to be missing from it (during
/// pairing the store is empty while live messages already populate the UI),
/// and the conversation being *read* is not yanked out from under its reader.
///
/// Read, not merely selected. The selection is deliberately kept while the
/// window is in Status, in Settings, under the fullscreen viewer or, on a
/// phone, walking the chat list — so sparing on the selection spared a chat
/// nobody was looking at, and left it in the sidebar until some other
/// conversation was picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Survival {
    /// Still a chat this window should show.
    Keep,
    /// Gone from the store, but on screen. Kept for now and owed a removal
    /// the moment it stops being drawn.
    Defer,
    /// Gone, and nobody is looking at it.
    Drop,
}

/// Apply that rule to one chat.
pub fn survives_complete_load(
    chat: &Chat,
    loaded: &std::collections::HashSet<&str>,
    visible: Option<&str>,
) -> Survival {
    if !chat.is_from_store() || loaded.contains(chat.jid.as_str()) {
        Survival::Keep
    } else if visible == Some(chat.jid.as_str()) {
        Survival::Defer
    } else {
        Survival::Drop
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

impl WhatsAppApp {
    /// Fold hydrated chats into the list.
    ///
    /// The shared half of every read that produces chats — a store load, a
    /// page of the list, the rows a snapshot painted — because all three
    /// arrive at a list that may already hold the same conversation. Merging
    /// rather than replacing is what keeps a live bubble that has not reached
    /// the store yet, and what spends the reads a row without messages could
    /// not bound.
    ///
    /// Never prunes: absence is a claim only a complete load may make, and
    /// only the caller knows whether this was one.
    pub(super) fn merge_chats(&mut self, chats: Vec<Chat>) {
        for chat in chats {
            match self
                .chats
                .iter_mut()
                .find(|c| c.jid == chat.jid)
                .map(Arc::make_mut)
            {
                Some(existing) => {
                    let jid = chat.jid.clone();
                    existing.merge_history(chat);
                    // The chat *on screen* was read locally the moment the
                    // message arrived; the store row commits with the unread
                    // bump before our receipt lands, so the hydrated counter
                    // must not resurrect the badge. On screen, not selected —
                    // the same distinction the live arrival makes, and for the
                    // same reason: a reload while the reader is in Status
                    // would otherwise clear the badge of a conversation nobody
                    // was looking at.
                    if self.visible_chat.as_deref() == Some(jid.as_str()) {
                        existing.mark_as_read();
                    }
                    // The read a row without messages could not bound. Spent
                    // here because this is what gave it a message to name; see
                    // `owed_reads`.
                    if self.owed_reads.contains(&jid)
                        && let Some(newest) = newest_shared_message(existing)
                    {
                        self.owed_reads.remove(&jid);
                        if let Some(client) = &self.client {
                            info!(
                                "Marking {} read, now that it has messages",
                                observe_str(&jid)
                            );
                            client.mark_chat_read(&jid, Some(newest));
                        }
                    }
                    self.invalidate_message_cache(&jid);
                }
                None => self.chats.push(Arc::new(chat)),
            }
        }
        self.chats
            .sort_by_key(|c| std::cmp::Reverse(c.last_message_time));
    }
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

    fn from_store(jid: &str) -> Chat {
        Chat::from_store(jid.to_string(), "Someone".to_string(), 0)
    }

    #[test]
    fn a_chat_the_store_still_has_stays() {
        let loaded = std::collections::HashSet::from(["a@s.whatsapp.net"]);
        assert_eq!(
            survives_complete_load(&from_store("a@s.whatsapp.net"), &loaded, None),
            Survival::Keep
        );
    }

    #[test]
    fn a_live_only_chat_is_not_the_stores_to_delete() {
        let loaded = std::collections::HashSet::new();
        assert_eq!(
            survives_complete_load(&chat("a@s.whatsapp.net", false, 0, false), &loaded, None),
            Survival::Keep,
            "during pairing the store is empty while live messages already exist"
        );
    }

    #[test]
    fn a_stored_chat_missing_from_a_complete_load_is_gone() {
        let loaded = std::collections::HashSet::from(["b@s.whatsapp.net"]);
        assert_eq!(
            survives_complete_load(&from_store("a@s.whatsapp.net"), &loaded, None),
            Survival::Drop
        );
    }

    #[test]
    fn the_conversation_on_screen_is_spared_but_owed_a_removal() {
        let loaded = std::collections::HashSet::from(["b@s.whatsapp.net"]);
        assert_eq!(
            survives_complete_load(
                &from_store("a@s.whatsapp.net"),
                &loaded,
                Some("a@s.whatsapp.net")
            ),
            Survival::Defer,
            "spared only because it is being read — not forgiven"
        );
    }

    #[test]
    fn a_chat_nobody_is_looking_at_goes_even_if_it_is_selected() {
        let loaded = std::collections::HashSet::from(["b@s.whatsapp.net"]);
        assert_eq!(
            survives_complete_load(&from_store("a@s.whatsapp.net"), &loaded, None),
            Survival::Drop,
            "the selection survives a trip to Status; being drawn does not"
        );
    }

    #[test]
    fn filter_ids_are_stable_and_distinct() {
        let ids: Vec<&str> = ChatFilter::ALL.iter().map(|f| f.id()).collect();
        assert_eq!(ids, vec!["all", "unread", "groups"]);
    }
}
