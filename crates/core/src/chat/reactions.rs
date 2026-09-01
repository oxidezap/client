//! Who reacted to what, and the ceilings that keep one message from growing
//! without bound.

use super::Chat;
use super::message::ChatMessage;

/// Maximum number of unique emoji reactions per message to prevent spam
const MAX_REACTIONS_PER_MESSAGE: usize = 50;

/// Maximum reactors one message will record, across every emoji on it.
///
/// The emoji count was bounded and the list under each was not, so one
/// message grew by a name per reaction — and reactions arrive from the
/// network with the sender in the envelope, so the same emoji from a
/// thousand JIDs is a row that is serialized into every history load and
/// copied into every status rebuild. Generous, because a large group really
/// does react.
const MAX_REACTORS_PER_MESSAGE: usize = 500;

impl ChatMessage {
    /// Add or update a reaction to this message from a sender.
    /// Each sender can only have one reaction - adding a new one removes the previous.
    /// An empty emoji string removes the sender's reaction entirely.
    pub fn add_reaction(&mut self, emoji: String, sender: String) {
        // Enforce the limit BEFORE removing the sender's old reaction: a
        // rejected replacement must not erase what they already had.
        if !emoji.is_empty()
            && !self.reactions.contains_key(&emoji)
            && self.reactions.len() >= MAX_REACTIONS_PER_MESSAGE
        {
            let frees_a_slot = self
                .reactions
                .values()
                .any(|senders| senders.len() == 1 && senders.contains(&sender));
            if !frees_a_slot {
                return;
            }
        }

        // The other half of the same bound: a distinct sender is a name this
        // message keeps, and a message with no ceiling on those is one the
        // network can grow without limit. Somebody who already reacted is
        // changing their mind rather than adding to it, so they are never
        // turned away.
        if !emoji.is_empty()
            && self.reactors() >= MAX_REACTORS_PER_MESSAGE
            && !self
                .reactions
                .values()
                .any(|senders| senders.contains(&sender))
        {
            return;
        }

        // Remove any existing reaction from this sender (one reaction per person)
        for senders in self.reactions.values_mut() {
            senders.retain(|s| s != &sender);
        }
        self.reactions.retain(|_, senders| !senders.is_empty());

        // Empty emoji means remove reaction
        if emoji.is_empty() {
            return;
        }

        self.reactions.entry(emoji).or_default().push(sender);
    }

    /// How many people have reacted to this message.
    fn reactors(&self) -> usize {
        self.reactions.values().map(Vec::len).sum()
    }
}

impl Chat {
    /// Add a reaction to a message in this chat
    ///
    /// Returns true if the message was found and the reaction was added.
    pub fn add_reaction(&mut self, message_id: &str, emoji: String, sender: String) -> bool {
        if let Some(msg) = self.messages.iter_mut().find(|m| m.id == message_id) {
            msg.add_reaction(emoji, sender);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::message::make_message;

    /// The emoji count was bounded and the reactor list under each was not,
    /// so one message grew by a name for every reaction the network carried —
    /// and that row is serialized into every history load.
    #[test]
    fn one_message_does_not_grow_a_name_per_reaction_forever() {
        let mut message = make_message("3EB0AAA", 1000);
        for reactor in 0..MAX_REACTORS_PER_MESSAGE + 200 {
            message.add_reaction("👍".to_string(), format!("{reactor}@s.whatsapp.net"));
        }
        assert_eq!(message.reactors(), MAX_REACTORS_PER_MESSAGE);

        // Somebody already counted is changing their mind, not adding to it.
        message.add_reaction("🎉".to_string(), "0@s.whatsapp.net".to_string());
        assert_eq!(message.reactors(), MAX_REACTORS_PER_MESSAGE);
        assert_eq!(
            message.reactions.get("🎉").map(Vec::len),
            Some(1),
            "and their new emoji is the one recorded"
        );
    }

    // Reaction tests

    #[test]
    fn test_add_single_reaction_to_message() {
        let mut msg = make_message("msg1", 1000);

        msg.add_reaction("👍".to_string(), "user1".to_string());

        assert_eq!(msg.reactions.len(), 1);
        assert!(msg.reactions.contains_key("👍"));
        assert_eq!(msg.reactions.get("👍").unwrap(), &vec!["user1".to_string()]);
    }

    #[test]
    fn test_add_multiple_different_reactions_to_message() {
        let mut msg = make_message("msg1", 1000);

        msg.add_reaction("👍".to_string(), "user1".to_string());
        msg.add_reaction("❤️".to_string(), "user2".to_string());
        msg.add_reaction("😂".to_string(), "user3".to_string());

        assert_eq!(msg.reactions.len(), 3);
        assert!(msg.reactions.contains_key("👍"));
        assert!(msg.reactions.contains_key("❤️"));
        assert!(msg.reactions.contains_key("😂"));
    }

    #[test]
    fn test_multiple_users_same_reaction() {
        let mut msg = make_message("msg1", 1000);

        msg.add_reaction("👍".to_string(), "user1".to_string());
        msg.add_reaction("👍".to_string(), "user2".to_string());
        msg.add_reaction("👍".to_string(), "user3".to_string());

        assert_eq!(msg.reactions.len(), 1);
        let senders = msg.reactions.get("👍").unwrap();
        assert_eq!(senders.len(), 3);
        assert!(senders.contains(&"user1".to_string()));
        assert!(senders.contains(&"user2".to_string()));
        assert!(senders.contains(&"user3".to_string()));
    }

    #[test]
    fn test_user_changes_reaction() {
        let mut msg = make_message("msg1", 1000);

        // User1 reacts with 👍
        msg.add_reaction("👍".to_string(), "user1".to_string());
        assert_eq!(msg.reactions.get("👍").unwrap().len(), 1);

        // User1 changes to ❤️
        msg.add_reaction("❤️".to_string(), "user1".to_string());

        // 👍 should be removed (empty), ❤️ should have user1
        assert!(!msg.reactions.contains_key("👍"));
        assert!(msg.reactions.contains_key("❤️"));
        assert_eq!(msg.reactions.get("❤️").unwrap(), &vec!["user1".to_string()]);
    }

    #[test]
    fn test_user_removes_reaction_with_empty_string() {
        let mut msg = make_message("msg1", 1000);

        msg.add_reaction("👍".to_string(), "user1".to_string());
        assert_eq!(msg.reactions.len(), 1);

        // Remove reaction by sending empty emoji
        msg.add_reaction("".to_string(), "user1".to_string());

        assert_eq!(msg.reactions.len(), 0);
    }

    #[test]
    fn test_chat_add_reaction() {
        let mut chat = Chat::new("test@s.whatsapp.net".to_string());
        chat.add_message(make_message("msg1", 1000));
        chat.add_message(make_message("msg2", 2000));

        // Add reaction to msg1
        let found = chat.add_reaction("msg1", "👍".to_string(), "user1".to_string());
        assert!(found);
        assert_eq!(chat.messages[0].reactions.len(), 1);

        // Add reaction to msg2
        let found = chat.add_reaction("msg2", "❤️".to_string(), "user2".to_string());
        assert!(found);
        assert_eq!(chat.messages[1].reactions.len(), 1);
    }

    #[test]
    fn test_chat_add_reaction_message_not_found() {
        let mut chat = Chat::new("test@s.whatsapp.net".to_string());
        chat.add_message(make_message("msg1", 1000));

        // Try to add reaction to non-existent message
        let found = chat.add_reaction("nonexistent", "👍".to_string(), "user1".to_string());
        assert!(!found);
    }

    #[test]
    fn test_reaction_count_multiple_emojis() {
        let mut msg = make_message("msg1", 1000);

        // 3 users react with 👍
        msg.add_reaction("👍".to_string(), "user1".to_string());
        msg.add_reaction("👍".to_string(), "user2".to_string());
        msg.add_reaction("👍".to_string(), "user3".to_string());

        // 2 users react with ❤️
        msg.add_reaction("❤️".to_string(), "user4".to_string());
        msg.add_reaction("❤️".to_string(), "user5".to_string());

        // 1 user reacts with 😂
        msg.add_reaction("😂".to_string(), "user6".to_string());

        assert_eq!(msg.reactions.len(), 3);
        assert_eq!(msg.reactions.get("👍").unwrap().len(), 3);
        assert_eq!(msg.reactions.get("❤️").unwrap().len(), 2);
        assert_eq!(msg.reactions.get("😂").unwrap().len(), 1);
    }
}
