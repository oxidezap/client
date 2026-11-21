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
