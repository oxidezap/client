//! Chat and message state structures
//!
//! Split by what the pieces are about rather than by type: the conversation
//! and the names in it here, one row in [`message`], the media hanging off a
//! row in [`media`], reactions in [`reactions`], and in [`merge`] the rules
//! for folding arriving traffic and hydrated history into a timeline.

mod media;
mod merge;
mod message;
mod reactions;

pub use media::{DownloadableMedia, MediaContent, MediaType, OutgoingMedia};
pub use message::{ChatMessage, Resend};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use wacore_binary::jid::{Jid, JidExt};

use crate::quoted::QuotedMessage;

pub fn fallback_chat_name(jid: &Jid) -> String {
    if jid.is_status_broadcast() {
        "Status".to_string()
    } else if jid.is_group() {
        "Unnamed group".to_string()
    } else if jid.is_broadcast_list() {
        "Broadcast list".to_string()
    } else if jid.is_newsletter() {
        "Channel".to_string()
    } else if jid.server.is_lid_family() {
        "Unknown contact".to_string()
    } else if jid.server.is_pn_family() && jid.user_base().chars().all(|c| c.is_ascii_digit()) {
        format!("+{}", jid.user_base())
    } else {
        "Unknown chat".to_string()
    }
}

/// The address every status update is addressed to.
pub const STATUS_BROADCAST_JID: &str = "status@broadcast";

/// Whether `jid` is the status broadcast.
///
/// A string compare rather than a parse: the address is a single fixed
/// constant, and both chat constructors are on paths that otherwise never
/// need a parsed JID.
fn is_status_jid(jid: &str) -> bool {
    jid == STATUS_BROADCAST_JID
}

/// A field whose absence means what its default does.
///
/// Paired with `#[serde(default)]` on the way back in, which is what makes
/// leaving it out safe: the reader fills in the same value the writer skipped.
/// A history load is a hundred chats of fifty rows and most of those fields
/// are empty on most of them, so what is skipped is about a third of the
/// frame — bytes to write, bytes to read, and two passes of serde over both.
fn is_false(value: &bool) -> bool {
    !*value
}

/// A chat/conversation
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chat {
    /// JID (Jabber ID) - unique identifier
    pub jid: String,
    /// Display name
    pub name: String,
    /// Fallback < history < live push name < address-book contact.
    pub(crate) name_priority: u8,
    /// Last message preview
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message: Option<String>,
    /// Time of last message
    pub last_message_time: Option<DateTime<Utc>>,
    /// Number of unread messages
    pub unread_count: u32,
    /// Manually marked unread (WA's `-1` sentinel): badge without a count.
    pub manually_unread: bool,
    /// Whether this is a group chat
    pub is_group: bool,
    /// Whether this is the status broadcast.
    ///
    /// Not a conversation: nothing is addressed to it and nothing is replied
    /// to in it — every message is one contact's status update, and the whole
    /// thing belongs on its own screen rather than in a list of people to
    /// talk to.
    #[serde(default)]
    pub is_status: bool,
    /// Participant names in group chats (sender JID -> display name)
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub participants: HashMap<String, String>,
    /// Messages in this chat
    pub messages: Vec<ChatMessage>,
    /// Whether the durable store has ever handed us this chat. Live-created
    /// chats (incoming message before its store row commits — e.g. the
    /// initial-pairing window) stay `false` until a history load adopts them,
    /// so a complete-but-still-empty store load must not prune them; a chat
    /// the store DID originate and no longer returns was deleted/archived
    /// elsewhere and must go.
    pub(crate) from_store: bool,
}

impl Chat {
    /// Create a new chat from a JID
    pub fn new(jid: String) -> Self {
        let name = jid
            .parse::<Jid>()
            .map(|jid| fallback_chat_name(&jid))
            .unwrap_or_else(|_| "Unknown chat".to_string());
        // Priority 0: a name nobody chose, which anything else outranks.
        Self::with_name_priority(jid, name, 0)
    }

    /// Create a new chat with a custom name
    #[allow(dead_code)]
    pub fn with_name(jid: String, name: String) -> Self {
        Self::with_name_priority(jid, name, 2)
    }

    /// Whether this chat was hydrated from the persistent store rather than
    /// created by live traffic. Only a store-originated chat may be pruned
    /// when a complete load stops returning it.
    pub fn is_from_store(&self) -> bool {
        self.from_store
    }

    /// Build a chat hydrated from the persistent store.
    ///
    /// Distinct from [`with_name`](Self::with_name) because the resulting chat
    /// carries the store origin: a later complete load that no longer returns
    /// this JID is allowed to prune it, which must never happen to a chat that
    /// only ever existed as live traffic.
    pub fn from_store(jid: String, name: String, name_priority: u8) -> Self {
        Self {
            from_store: true,
            ..Self::with_name_priority(jid, name, name_priority)
        }
    }

    pub(crate) fn with_name_priority(jid: String, name: String, name_priority: u8) -> Self {
        let is_group = jid.contains("@g.us");
        let is_status = is_status_jid(&jid);

        Self {
            jid,
            name,
            name_priority,
            last_message: None,
            last_message_time: None,
            unread_count: 0,
            manually_unread: false,
            is_group,
            is_status,
            participants: HashMap::new(),
            messages: Vec::new(),
            from_store: false,
        }
    }

    /// Adopt `name` only when it comes from a higher-priority source than the
    /// one already stored. Front ends and the session layer both learn names
    /// from several sources (history, pushname, group metadata) whose
    /// precedence must not depend on arrival order.
    pub fn set_name_if_better(&mut self, name: String, priority: u8) {
        if priority > self.name_priority {
            self.name = name;
            self.name_priority = priority;
        }
    }

    /// Like [`set_name_if_better`](Self::set_name_if_better), but a same-priority
    /// name wins. Used where the newer value from an equally trusted source is
    /// the more current one.
    pub fn set_name_if_not_worse(&mut self, name: String, priority: u8) {
        if priority >= self.name_priority {
            self.name = name;
            self.name_priority = priority;
        }
    }

    /// Learn what to call a participant, and say it everywhere this chat
    /// already names them.
    ///
    /// The one place a name enters a conversation, which is what keeps the
    /// bubble, the quote bar above it and the row in the list from drifting
    /// apart: a name that arrives late is written onto the rows that were
    /// waiting for it rather than left to whichever surface happens to
    /// consult the participant map. Both back-fills only fill blanks — a row
    /// that already carries a name was named by the same resolver and is not
    /// improved by a second answer.
    pub fn update_participant(&mut self, jid: String, name: String) {
        for message in &mut self.messages {
            if let Some(quoted) = message.quoted.as_mut()
                && quoted.sender_name.is_empty()
                && quoted.sender == jid
            {
                quoted.sender_name.clone_from(&name);
            }
            if message.sender_name.is_none() && !message.is_from_me && message.sender == jid {
                message.sender_name = Some(name.clone());
            }
        }
        self.participants.insert(jid, name);
    }

    /// What to call whoever wrote `message`, in this chat.
    ///
    /// Every surface that names an author asks this, so a bubble, the list's
    /// preview prefix and a reply bar cannot answer differently: the name the
    /// message arrived under, then the participant map, and `None` when
    /// nobody here knows. What to draw instead of `None` is the caller's —
    /// the list says nothing, the reply bar names the chat.
    pub fn author_name<'a>(&'a self, message: &'a ChatMessage) -> Option<&'a str> {
        message
            .sender_name
            .as_deref()
            .or_else(|| self.participant_name(&message.sender))
    }

    /// What this chat calls whoever is at `jid`, if it knows.
    pub fn participant_name(&self, jid: &str) -> Option<&str> {
        self.participants.get(jid).map(String::as_str)
    }

    /// Give a reply's quote bar the name of whoever it is answering.
    ///
    /// The envelope carries the quoted author's JID and never their push
    /// name, so this is the only place the two meet: the chat holds the
    /// participant map, and its own name is the answer in a 1:1 where there
    /// is no map to hold.
    fn name_quoted_author(&self, message: &mut ChatMessage) {
        let Some(quoted) = message.quoted.as_mut() else {
            return;
        };
        if !quoted.sender_name.is_empty() {
            return;
        }
        if let Some(name) = self.quoted_author(quoted) {
            quoted.sender_name = name;
        }
    }

    /// Give every loaded reply the name of whoever it is answering.
    ///
    /// A page of hydrated history is assigned wholesale rather than added a
    /// row at a time, so the naming [`add_message`](Self::add_message) does
    /// per row has to be run over the page afterwards — without it every
    /// reloaded reply drew the generic "Message" where an author belonged.
    pub fn name_quoted_authors(&mut self) {
        // Resolved against the whole page first: the answer for one row can
        // come from another row, and both borrows cannot be held at once.
        let resolved: Vec<Option<String>> = self
            .messages
            .iter()
            .map(|message| {
                let quoted = message.quoted.as_ref()?;
                quoted
                    .sender_name
                    .is_empty()
                    .then(|| self.quoted_author(quoted))
                    .flatten()
            })
            .collect();
        for (message, name) in self.messages.iter_mut().zip(resolved) {
            if let Some(quoted) = message.quoted.as_mut()
                && let Some(name) = name
            {
                quoted.sender_name = name;
            }
        }
    }

    /// Who wrote the message a quote is quoting, as far as this chat knows.
    fn quoted_author(&self, quoted: &QuotedMessage) -> Option<String> {
        // The original is often still loaded, and it is the better answer:
        // it knows whether the reader wrote it, and carries the name it was
        // received under.
        if let Some(original) = self
            .messages
            .iter()
            .find(|message| message.id == quoted.message_id)
        {
            if original.is_from_me {
                return Some("You".to_string());
            }
            if let Some(name) = &original.sender_name {
                return Some(name.clone());
            }
        }
        // No participant on the envelope and no loaded original: nobody here
        // knows who wrote it. Naming the chat would be a guess, and in a 1:1
        // it is wrong exactly when the reader quoted themselves — the quote
        // bar falls back to its own generic label instead.
        if quoted.sender.is_empty() {
            return None;
        }
        if let Some(name) = self.participant_name(&quoted.sender) {
            Some(name.to_string())
        } else if !self.is_group {
            Some(self.name.clone())
        } else {
            // Better than "Message": a number is at least *someone*, and the
            // real name replaces it as soon as a push name arrives.
            quoted
                .sender
                .parse::<Jid>()
                .ok()
                .map(|jid| fallback_chat_name(&jid))
        }
    }

    /// Get the initial letter for avatar display
    #[allow(dead_code)]
    pub fn initial(&self) -> char {
        self.name.chars().next().unwrap_or('?')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One person, one name. The bug: the same participant was "Eu" on their
    /// bubbles and "jlucaso" in the typing line, because the two surfaces
    /// asked different sources. Everything that names an author asks here.
    #[test]
    fn every_surface_names_an_author_the_same_way() {
        let mut chat = Chat::new("120363000000000001@g.us".to_string());
        chat.is_group = true;

        let mut named = ChatMessage::new_incoming("m1".into(), "a@lid".into(), "ping".into());
        named.sender_name = Some("Eu".into());
        let anonymous = ChatMessage::new_incoming("m2".into(), "a@lid".into(), "pong".into());

        assert_eq!(chat.author_name(&named), Some("Eu"));
        assert_eq!(chat.author_name(&anonymous), None, "nobody here knows yet");

        chat.update_participant("a@lid".into(), "Eu".into());
        assert_eq!(chat.author_name(&anonymous), Some("Eu"));
        assert_eq!(chat.participant_name("a@lid"), Some("Eu"));
        assert_eq!(chat.participant_name("b@lid"), None);
    }

    /// A name that arrives after the rows it belongs to is written onto them,
    /// so a bubble and the list row above it cannot disagree about who spoke.
    #[test]
    fn a_name_learned_late_reaches_the_rows_that_were_waiting() {
        let mut chat = Chat::new("120363000000000001@g.us".to_string());
        chat.is_group = true;
        chat.add_message(ChatMessage::new_incoming(
            "m1".into(),
            "a@lid".into(),
            "ping".into(),
        ));
        let mut theirs = ChatMessage::new_incoming("m2".into(), "b@lid".into(), "pong".into());
        theirs.sender_name = Some("Ana".into());
        chat.add_message(theirs);
        chat.add_message(ChatMessage::new_outgoing("m3".into(), "ok".into()));

        chat.update_participant("a@lid".into(), "Eu".into());

        assert_eq!(chat.messages[0].sender_name.as_deref(), Some("Eu"));
        assert_eq!(
            chat.messages[1].sender_name.as_deref(),
            Some("Ana"),
            "somebody else's row is not touched"
        );
        assert_eq!(
            chat.messages[2].sender_name, None,
            "your own row is named by the reader, not by the map"
        );
    }

    /// A page of history is assigned wholesale, not added a row at a time, so
    /// the per-row naming has to be run over it afterwards. Without it every
    /// reloaded reply drew the generic label where an author belonged.
    #[test]
    fn a_reloaded_reply_still_names_who_it_answers() {
        let mut chat = Chat::new("120363000000000001@g.us".to_string());
        chat.is_group = true;

        let mut original = ChatMessage::new_incoming("m1".into(), "a@lid".into(), "ping".into());
        original.sender_name = Some("Ana".into());
        let mut reply = ChatMessage::new_incoming("m2".into(), "b@lid".into(), "pong".into());
        reply.quoted = Some(crate::QuotedMessage {
            message_id: "m1".into(),
            sender: "a@lid".into(),
            sender_name: String::new(),
            preview: "ping".into(),
            kind: None,
        });
        // Assigned the way hydration assigns a page.
        chat.messages = vec![original, reply];

        chat.name_quoted_authors();
        assert_eq!(chat.messages[1].quoted.as_ref().unwrap().sender_name, "Ana");
    }

    #[test]
    fn chat_fallback_hides_internal_lid() {
        let lid = Chat::new("111222333444555@lid".to_string());
        let pn = Chat::new("12025550143@s.whatsapp.net".to_string());
        let legacy = Chat::new("12025550144@c.us".to_string());

        assert_eq!(lid.name, "Unknown contact");
        assert_eq!(pn.name, "+12025550143");
        assert_eq!(legacy.name, "+12025550144");
    }

    /// What one frame of a long conversation used to copy.
    ///
    /// A stopwatch rather than an assertion, so it is ignored by default. The
    /// conversation view held a `&Chat` across calls that need the app
    /// mutably, so it cloned the whole chat — every message with its text,
    /// its reaction map and its quote — on every frame, for readers that only
    /// ever look at the timeline cache.
    ///
    /// `cargo test -p oxidezap-core --release -- --ignored conversation_clone_cost`
    #[test]
    #[ignore = "a stopwatch, not a test"]
    fn conversation_clone_cost() {
        for rows in [500usize, 1_000, 2_000, 5_000] {
            let mut chat = Chat::new("a@s.whatsapp.net".to_string());
            chat.messages = (0..rows)
                .map(|n| {
                    let mut message = ChatMessage::new_incoming(
                        format!("MSG-{n}"),
                        "a@s.whatsapp.net".to_string(),
                        "uma mensagem de tamanho bastante comum".to_string(),
                    );
                    message
                        .reactions
                        .insert("👍".to_string(), vec!["a@s.whatsapp.net".to_string()]);
                    message
                })
                .collect();

            let frames = 60u32;
            let started = wacore::time::Instant::now();
            for _ in 0..frames {
                std::hint::black_box(chat.clone());
            }
            println!(
                "{rows} messages: {:?} per frame",
                started.elapsed() / frames
            );
        }
    }
}
