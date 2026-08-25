//! Status updates, grouped by who posted them.
//!
//! Every status update on the account arrives in one conversation —
//! `status@broadcast` — addressed to nobody in particular. Read as a chat that
//! is nonsense: it has no other party, its "messages" are not to you, and a
//! reply would not go where the bubble suggests. Read as a feed grouped by
//! author it is what it actually is, which is why this type exists and why the
//! broadcast is kept out of the conversation list.
//!
//! The grouping holds indices into the message slice rather than copies of it:
//! a feed is rebuilt whenever the conversation grows, and one rebuild should
//! not clone every image caption on the account.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use wacore_binary::jid::Jid;

use crate::chat::{Chat, ChatMessage, fallback_chat_name};

/// One contact's run of updates, newest run first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusAuthor {
    /// Who posted. Empty for our own updates, which have no sender to name.
    pub jid: String,
    pub name: String,
    /// Indices into [`StatusFeed::messages`], oldest first — the order they
    /// are played back in.
    pub updates: Vec<usize>,
    /// When the most recent of them arrived, which is what the row shows and
    /// what the list is ordered by.
    pub latest: DateTime<Utc>,
    /// How many of them have not been looked at yet.
    pub unseen: usize,
}

impl StatusAuthor {
    pub fn count(&self) -> usize {
        self.updates.len()
    }

    pub fn has_unseen(&self) -> bool {
        self.unseen > 0
    }
}

/// The status broadcast, as a feed rather than a conversation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusFeed {
    messages: Arc<[ChatMessage]>,
    /// Our own updates, gathered separately: they head the screen rather than
    /// taking a place in the list of contacts.
    mine: Option<StatusAuthor>,
    authors: Vec<StatusAuthor>,
}

impl StatusFeed {
    /// Group `chat`'s messages by author. Cheap enough to run per rebuild: one
    /// pass, one small vector per author, and no message is copied.
    pub fn from_chat(chat: &Chat) -> Self {
        Self::from_messages(&chat.messages, chat)
    }

    fn from_messages(messages: &[ChatMessage], chat: &Chat) -> Self {
        let mut mine: Option<StatusAuthor> = None;
        // A handful of contacts post on a given day, so a linear scan over the
        // authors found so far beats a map: no hashing, no allocation per
        // lookup, and the vector is the answer the caller wants anyway.
        let mut authors: Vec<StatusAuthor> = Vec::new();

        for (index, message) in messages.iter().enumerate() {
            // A call record or a group notice cannot be a status update, and
            // neither can an empty row.
            if message.system.is_some() {
                continue;
            }

            let slot = if message.is_from_me {
                mine.get_or_insert_with(|| StatusAuthor {
                    jid: String::new(),
                    name: "My status".to_string(),
                    updates: Vec::new(),
                    latest: message.timestamp,
                    unseen: 0,
                })
            } else {
                let jid = message.sender.as_str();
                match authors.iter().position(|author| author.jid == jid) {
                    Some(at) => &mut authors[at],
                    None => {
                        authors.push(StatusAuthor {
                            jid: jid.to_string(),
                            name: author_name(message, chat),
                            updates: Vec::new(),
                            latest: message.timestamp,
                            unseen: 0,
                        });
                        authors.last_mut().expect("just pushed")
                    }
                }
            };

            slot.updates.push(index);
            slot.latest = slot.latest.max(message.timestamp);
            if !message.is_read && !message.is_from_me {
                slot.unseen += 1;
            }
        }

        // Newest first, and unseen ahead of seen — the same order WhatsApp
        // itself uses, and the one that puts what you have not watched where
        // you will find it.
        authors.sort_by(|a, b| {
            b.has_unseen()
                .cmp(&a.has_unseen())
                .then(b.latest.cmp(&a.latest))
        });

        Self {
            messages: Arc::from(messages.to_vec()),
            mine,
            authors,
        }
    }

    pub fn messages(&self) -> &Arc<[ChatMessage]> {
        &self.messages
    }

    pub fn mine(&self) -> Option<&StatusAuthor> {
        self.mine.as_ref()
    }

    pub fn authors(&self) -> &[StatusAuthor] {
        &self.authors
    }

    pub fn is_empty(&self) -> bool {
        self.mine.is_none() && self.authors.is_empty()
    }

    /// How many contacts have something unwatched, which is what the
    /// navigation badge counts.
    pub fn unseen_authors(&self) -> usize {
        self.authors
            .iter()
            .filter(|author| author.has_unseen())
            .count()
    }

    /// The author a selection names. An empty JID is our own updates, which is
    /// why it is not simply a lookup by JID.
    pub fn author(&self, jid: &str) -> Option<&StatusAuthor> {
        if jid.is_empty() {
            return self.mine.as_ref();
        }
        self.authors.iter().find(|author| author.jid == jid)
    }

    /// The messages of one author, in playback order.
    pub fn updates_of<'a>(
        &'a self,
        author: &'a StatusAuthor,
    ) -> impl Iterator<Item = &'a ChatMessage> + 'a {
        author
            .updates
            .iter()
            .filter_map(|ix| self.messages.get(*ix))
    }
}

/// The best name available for whoever posted, without inventing one: the push
/// name on the update, then whatever the broadcast learned about them, then the
/// bare address.
fn author_name(message: &ChatMessage, chat: &Chat) -> String {
    if let Some(name) = message.sender_name.as_ref().filter(|n| !n.is_empty()) {
        return name.clone();
    }
    if let Some(name) = chat
        .participants
        .get(&message.sender)
        .filter(|name| !name.is_empty())
    {
        return name.clone();
    }
    // Whatever is left, through the same fallback every other unnamed JID in
    // the app goes through. Prefixing the user part with "+" was wrong for the
    // common case here: a status arrives from a LID, which is not a phone
    // number, and printing it as one invents a number nobody can dial.
    message
        .sender
        .parse::<Jid>()
        .map(|jid| fallback_chat_name(&jid))
        .unwrap_or_else(|_| "Unknown contact".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::STATUS_BROADCAST_JID;
    use chrono::TimeZone as _;

    fn at(minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 25, 12, minute, 0).unwrap()
    }

    fn update(id: &str, sender: &str, name: Option<&str>, minute: u32, read: bool) -> ChatMessage {
        let mut message = ChatMessage::new_outgoing(id.to_string(), "hi".to_string());
        message.is_from_me = false;
        message.sender = sender.to_string();
        message.sender_name = name.map(str::to_string);
        message.timestamp = at(minute);
        message.is_read = read;
        message
    }

    fn broadcast(messages: Vec<ChatMessage>) -> Chat {
        let mut chat = Chat::new(STATUS_BROADCAST_JID.to_string());
        chat.messages = messages;
        chat
    }

    #[test]
    fn the_broadcast_is_not_a_conversation() {
        let chat = Chat::new(STATUS_BROADCAST_JID.to_string());
        assert!(chat.is_status);
        assert!(!Chat::new("a@s.whatsapp.net".to_string()).is_status);
        assert!(!Chat::new("g@g.us".to_string()).is_status);
    }

    #[test]
    fn each_contact_appears_once_however_often_they_post() {
        let feed = StatusFeed::from_chat(&broadcast(vec![
            update("1", "a@s.whatsapp.net", Some("Ana"), 1, true),
            update("2", "a@s.whatsapp.net", Some("Ana"), 5, true),
            update("3", "m@s.whatsapp.net", Some("Marcos"), 3, true),
        ]));

        assert_eq!(feed.authors().len(), 2);
        let ana = feed.author("a@s.whatsapp.net").unwrap();
        assert_eq!(ana.count(), 2);
        assert_eq!(ana.latest, at(5));
    }

    #[test]
    fn unwatched_contacts_come_first_then_the_most_recent() {
        let feed = StatusFeed::from_chat(&broadcast(vec![
            update("1", "a@s.whatsapp.net", Some("Ana"), 9, true),
            update("2", "m@s.whatsapp.net", Some("Marcos"), 1, false),
        ]));

        let names: Vec<&str> = feed
            .authors()
            .iter()
            .map(|author| author.name.as_str())
            .collect();
        assert_eq!(names, vec!["Marcos", "Ana"]);
        assert_eq!(feed.unseen_authors(), 1);
    }

    #[test]
    fn our_own_updates_are_kept_out_of_the_contact_list() {
        let mut ours = update("1", "me", None, 2, true);
        ours.is_from_me = true;
        let feed = StatusFeed::from_chat(&broadcast(vec![
            ours,
            update("2", "a@s.whatsapp.net", Some("Ana"), 4, true),
        ]));

        assert_eq!(feed.authors().len(), 1);
        assert_eq!(feed.mine().unwrap().count(), 1);
        // Ours are never "unseen": we posted them.
        assert_eq!(feed.mine().unwrap().unseen, 0);
    }

    #[test]
    fn a_contact_who_never_sent_a_push_name_is_still_addressable() {
        let feed = StatusFeed::from_chat(&broadcast(vec![update(
            "1",
            "5511999999999@s.whatsapp.net",
            None,
            1,
            true,
        )]));
        assert_eq!(feed.authors()[0].name, "+5511999999999");

        // A LID is not a phone number and must not be printed as one.
        let lid = StatusFeed::from_chat(&broadcast(vec![update(
            "2",
            "39492358562039@lid",
            None,
            1,
            true,
        )]));
        assert_eq!(lid.authors()[0].name, "Unknown contact");
    }

    #[test]
    fn updates_play_back_oldest_first() {
        let feed = StatusFeed::from_chat(&broadcast(vec![
            update("1", "a@s.whatsapp.net", Some("Ana"), 1, true),
            update("2", "a@s.whatsapp.net", Some("Ana"), 7, true),
        ]));
        let ana = feed.author("a@s.whatsapp.net").unwrap();
        let ids: Vec<&str> = feed
            .updates_of(ana)
            .map(|message| message.id.as_str())
            .collect();
        assert_eq!(ids, vec!["1", "2"]);
    }

    #[test]
    fn a_call_record_is_not_an_update() {
        let mut notice = update("1", "a@s.whatsapp.net", Some("Ana"), 1, true);
        notice.system = Some(crate::SystemNotice::GroupChanged("changed".to_string()));
        let feed = StatusFeed::from_chat(&broadcast(vec![notice]));
        assert!(feed.is_empty());
    }
}
