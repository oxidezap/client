//! Who is in a group.
//!
//! Separate from [`Chat::participants`](crate::Chat::participants), which is
//! not a roster and never was: that map is filled as senders are *seen*, so a
//! fifty-person group with one recent sender has one entry in it. This is the
//! membership list itself, as the account's own connection reports it, and it
//! is the only thing that may be counted or listed as "who is in here".

use serde::{Deserialize, Serialize};

/// The members of one group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRoster {
    /// The group this describes.
    pub jid: String,
    /// Everyone in it, in the order the connection listed them, with this
    /// account's own entry marked rather than moved: where "You" goes in a
    /// line of names is the renderer's decision, not this type's.
    pub members: Vec<GroupMember>,
}

/// One member of a group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMember {
    /// How the group addresses them, which in a LID-addressed group is a LID
    /// and not a number.
    pub jid: String,
    /// What to call them, or `None` when nobody has ever said.
    ///
    /// Absent rather than filled with the number, for the reason
    /// [`ChatMessage::sender_name`](crate::ChatMessage::sender_name) is:
    /// drawing a stranger is the renderer's job, and a field that has already
    /// been given a number can never be improved by a name arriving later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Whether this member is the account reading it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_self: bool,
}

impl GroupRoster {
    /// How many people are in the group.
    #[must_use]
    pub fn size(&self) -> usize {
        self.members.len()
    }
}
