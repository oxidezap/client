//! Who is in a group, for the surfaces that name them.
//!
//! The membership list is not something this side can derive. A chat's
//! `participants` map fills as senders are *seen*, so it answers "who has
//! spoken here lately" and nothing else — a fifty-person group with one
//! recent sender has one entry in it, and the header that once counted them
//! said "1 members".
//!
//! The connection is what knows, and it already does: the library keeps a
//! participant list per group because the *send* path needs one, patches it
//! as membership notifications arrive, and invalidates it when the server
//! says its snapshot is stale. That list is what this asks for.

use whatsapp_rust::wacore_binary::jid::{Jid, JidExt as _};

use oxidezap_core::{GroupMember, GroupRoster};

use super::WhatsAppClient;
use super::history::own_jids;
use crate::exec::Task;

impl WhatsAppClient {
    /// Everyone in `jid`, named the way every other surface names them.
    ///
    /// Through [`Groups::query_info`], which is the cached, send-oriented
    /// view: a group that has been written to or read from since the last
    /// membership change is answered without touching the network, and a miss
    /// sends the participant hash so an unchanged group costs a
    /// `not-modified` rather than a full download. The fuller
    /// `Groups::get_metadata` — subject, description, admin roles — has no
    /// cache in front of it at all, and none of what it adds is drawn
    /// anywhere yet.
    ///
    /// Names come from the [`NameBook`](crate::names::NameBook) like a
    /// bubble's do, so the same person is not "Ana" over their message and a
    /// number in the line above it. A member nobody has ever named is
    /// returned nameless: drawing a stranger is the renderer's job, here for
    /// the reason it is everywhere else in this crate.
    pub fn group_roster(&self, jid: String) -> Task<Result<GroupRoster, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let group: Jid = jid.parse().map_err(|_| "not a chat address".to_string())?;
            if !group.is_group() {
                return Err("not a group".to_string());
            }
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let info = live
                .client
                .groups()
                .query_info(&group)
                .await
                .map_err(|e| e.to_string())?;
            // Both of this account's addresses, because a group addresses its
            // members by LID or by number depending on how it was created,
            // and "You" has to be recognised under either.
            let mine = own_jids(&live.client);
            let mut members = Vec::with_capacity(info.participants.len());
            for participant in &info.participants {
                let is_self = mine.contains(&participant.to_non_ad_string());
                // Not looked up for this account: it is drawn as "You", and
                // asking would put the owner's own address-book entry — or
                // their number — in a line about everybody else.
                let name = if is_self {
                    None
                } else {
                    live.names.known(&live.client, participant, None).await
                };
                members.push(GroupMember {
                    jid: participant.to_string(),
                    name,
                    is_self,
                });
            }
            Ok(GroupRoster {
                jid: group.to_string(),
                members,
            })
        })
    }
}
