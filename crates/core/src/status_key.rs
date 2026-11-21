//! Identity of an update in the combined status feed. The UI indexes rows by
//! one string; a wire stanza ID is only unique within its origin and author.

use serde::{Deserialize, Serialize};

const PREFIX: &str = "~status/";
const ENVELOPE: &str = "~status/post/";
const BROADCAST: &str = "~status/broadcast/";

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusPostKey {
    pub origin: String,
    pub sender: String,
    pub from_me: bool,
    pub id: String,
}

impl StatusPostKey {
    pub fn feed_id(&self) -> String {
        format!(
            "{ENVELOPE}{}",
            serde_json::to_string(self).expect("string fields")
        )
    }

    pub fn from_feed_id(id: &str) -> Option<Self> {
        serde_json::from_str(id.strip_prefix(ENVELOPE)?).ok()
    }

    /// Recover the original address before quoting a status or requesting its media.
    pub fn wire_key(feed_id: &str, sender: &str, from_me: bool) -> Self {
        Self::from_feed_id(feed_id).unwrap_or_else(|| Self {
            origin: crate::STATUS_BROADCAST_JID.to_owned(),
            sender: sender.to_owned(),
            from_me,
            id: Self::broadcast_wire_id(feed_id).to_owned(),
        })
    }

    /// Existing broadcast IDs keep their spelling unless it is reserved for
    /// a feed key. Even an ID containing a complete valid key stays distinct.
    pub fn broadcast_feed_id(id: String) -> String {
        if id.starts_with(PREFIX) {
            format!("{BROADCAST}{id}")
        } else {
            id
        }
    }

    pub fn broadcast_wire_id(id: &str) -> &str {
        id.strip_prefix(BROADCAST).unwrap_or(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_keys_round_trip_for_group_and_broadcast_even_if_ids_collide() {
        let key = StatusPostKey {
            origin: "120363000000000001@g.us".into(),
            sender: "10000000001@s.whatsapp.net".into(),
            from_me: false,
            id: "opaque/quoted-\"id\"".into(),
        };
        let feed_id = key.feed_id();
        assert_eq!(StatusPostKey::wire_key(&feed_id, "", false), key);
        let broadcast = StatusPostKey::wire_key(
            &StatusPostKey::broadcast_feed_id(feed_id.clone()),
            &key.sender,
            false,
        );
        assert_eq!(broadcast.id, feed_id);
        assert_eq!(broadcast.origin, crate::STATUS_BROADCAST_JID);
        assert_eq!(broadcast.sender, key.sender);
    }
}
