//! Authoritative community/group relationships supplied by the WhatsApp engine.
//!
//! `None` on [`super::Chat::group_hierarchy`] means the relationship is not
//! known yet; it is not evidence that the group is standalone. The JID, never
//! the display name, is the identity of a parent.

use serde::{Deserialize, Serialize};

/// A group's place in a community tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum GroupHierarchy {
    /// The server explicitly identifies this as an ordinary, unlinked group.
    Standalone,
    /// The server identifies this group as a community parent.
    Community,
    /// A group linked to a community parent.
    Subgroup {
        /// Stable identity of the parent community.
        parent_jid: String,
        /// The role this subgroup has within its community.
        kind: SubgroupKind,
    },
}

/// The role of a subgroup within its parent community.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubgroupKind {
    Regular,
    Announcement,
    General,
    /// A newer engine role that this client does not yet render specially.
    Other,
}

impl GroupHierarchy {
    /// The parent identity, if this group is a subgroup.
    #[must_use]
    pub fn parent_jid(&self) -> Option<&str> {
        match self {
            Self::Subgroup { parent_jid, .. } => Some(parent_jid),
            Self::Standalone | Self::Community => None,
        }
    }
}
