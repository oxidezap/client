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

    /// Every group this account participates in, by subject.
    ///
    /// The participating query always reaches the server, so there is no
    /// cached variant to opt out of and no refresh flag to honour: listing is
    /// the refresh.
    pub fn list_groups(&self) -> Task<Result<Vec<GroupListEntry>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let participating = live
                .client
                .groups()
                .get_participating()
                .await
                .map_err(|e| e.to_string())?;
            let mut groups: Vec<GroupListEntry> = participating
                .into_values()
                .map(|meta| GroupListEntry {
                    jid: meta.id.to_string(),
                    subject: meta.subject,
                    participant_count: meta.participants.len(),
                })
                .collect();
            groups.sort_by(|a, b| a.subject.cmp(&b.subject));
            Ok(groups)
        })
    }

    /// Full metadata for one group: subject, description, owner, roles.
    pub fn get_group_info(&self, group_jid: String) -> Task<Result<GroupDetails, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let group: Jid = group_jid
                .parse()
                .map_err(|_| "not a group address".to_string())?;
            if !group.is_group() {
                return Err("not a group".to_string());
            }
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let meta = live
                .client
                .groups()
                .get_metadata(&group)
                .await
                .map_err(|e| e.to_string())?;
            Ok(GroupDetails {
                jid: meta.id.to_string(),
                subject: meta.subject,
                description: meta.description,
                owner_jid: meta.creator.map(|j| j.to_string()),
                participant_count: meta.participants.len(),
                participants: meta
                    .participants
                    .iter()
                    .map(|p| GroupParticipantView {
                        jid: p.jid.to_string(),
                        is_admin: p.is_admin(),
                        is_superadmin: p.is_super_admin(),
                    })
                    .collect(),
                announce_only: meta.is_announcement,
                locked: meta.is_locked,
            })
        })
    }

    /// Create a group with phone numbers or JIDs as initial participants.
    ///
    /// Returns the new group's JID. Bare digit strings are read as phone
    /// numbers, like the CLI documents them.
    pub fn create_group(
        &self,
        subject: String,
        participants: Vec<String>,
    ) -> Task<Result<String, String>> {
        use whatsapp_rust::wacore::iq::groups::{GroupCreateOptions, GroupParticipantOptions};

        let session = self.session.clone();
        self.exec.spawn(async move {
            if subject.is_empty() {
                return Err("group subject must not be empty".to_string());
            }
            let mut options = Vec::with_capacity(participants.len());
            for raw in &participants {
                let jid = parse_participant(raw)?;
                options.push(GroupParticipantOptions::new(jid));
            }
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let created = live
                .client
                .groups()
                .create_group(GroupCreateOptions::new(subject).with_participants(options))
                .await
                .map_err(|e| e.to_string())?;
            Ok(created.metadata.id.to_string())
        })
    }

    /// Rename a group (its subject/topic).
    pub fn set_group_topic(&self, group_jid: String, topic: String) -> Task<Result<(), String>> {
        use whatsapp_rust::wacore::iq::groups::GroupSubject;

        let session = self.session.clone();
        self.exec.spawn(async move {
            let group: Jid = group_jid
                .parse()
                .map_err(|_| "not a group address".to_string())?;
            let subject = GroupSubject::new(topic).map_err(|e| e.to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            live.client
                .groups()
                .set_subject(&group, subject)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// Replace a group's description. An empty description clears it.
    pub fn set_group_description(
        &self,
        group_jid: String,
        description: String,
    ) -> Task<Result<(), String>> {
        use whatsapp_rust::features::PreviousDescription;
        use whatsapp_rust::wacore::iq::groups::GroupDescription;

        let session = self.session.clone();
        self.exec.spawn(async move {
            let group: Jid = group_jid
                .parse()
                .map_err(|_| "not a group address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let description = if description.is_empty() {
                None
            } else {
                Some(GroupDescription::new(description).map_err(|e| e.to_string())?)
            };
            live.client
                .groups()
                .set_description(&group, description, PreviousDescription::Resolve)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// Add, remove, promote or demote one participant.
    ///
    /// Returns how many of the server's per-participant answers succeeded:
    /// membership changes are partial by nature, and a bare unit would hide
    /// the half that did not apply.
    pub fn manage_group_participant(
        &self,
        group_jid: String,
        participant_jid: String,
        action: ParticipantChange,
    ) -> Task<Result<usize, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let group: Jid = group_jid
                .parse()
                .map_err(|_| "not a group address".to_string())?;
            let participant = parse_participant(&participant_jid)?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let groups = live.client.groups();
            let answers = match action {
                ParticipantChange::Add => groups
                    .add_participants(&group, &[participant])
                    .await
                    .map_err(|e| e.to_string())?,
                ParticipantChange::Remove => groups
                    .remove_participants(&group, &[participant])
                    .await
                    .map_err(|e| e.to_string())?,
                ParticipantChange::Promote => groups
                    .promote_participants(&group, &[participant])
                    .await
                    .map_err(|e| e.to_string())?,
                ParticipantChange::Demote => groups
                    .demote_participants(&group, &[participant])
                    .await
                    .map_err(|e| e.to_string())?,
            };
            Ok(answers.iter().filter(|a| a.is_ok()).count())
        })
    }

    /// Fetch (and optionally reset) a group's invite link.
    pub fn get_group_invite_link(
        &self,
        group_jid: String,
        reset: bool,
    ) -> Task<Result<String, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let group: Jid = group_jid
                .parse()
                .map_err(|_| "not a group address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            live.client
                .groups()
                .get_invite_link(&group, reset)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// Join a group through its invite code.
    pub fn join_group(&self, invite_code: String) -> Task<Result<JoinGroupView, String>> {
        use whatsapp_rust::wacore::iq::groups::JoinGroupResult;

        let session = self.session.clone();
        self.exec.spawn(async move {
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            match live
                .client
                .groups()
                .join_with_invite_code(&invite_code)
                .await
                .map_err(|e| e.to_string())?
            {
                JoinGroupResult::Joined(jid) => Ok(JoinGroupView {
                    jid: jid.to_string(),
                    pending_approval: false,
                }),
                JoinGroupResult::PendingApproval(jid) => Ok(JoinGroupView {
                    jid: jid.to_string(),
                    pending_approval: true,
                }),
            }
        })
    }

    /// Leave a group.
    pub fn leave_group(&self, group_jid: String) -> Task<Result<(), String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let group: Jid = group_jid
                .parse()
                .map_err(|_| "not a group address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            live.client
                .groups()
                .leave(group)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// Set announce-only (admins send) and locked (admins edit info) modes.
    pub fn set_group_permissions(
        &self,
        group_jid: String,
        announce_only: bool,
        locked: bool,
    ) -> Task<Result<(), String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let group: Jid = group_jid
                .parse()
                .map_err(|_| "not a group address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let groups = live.client.groups();
            groups
                .set_announce(&group, announce_only)
                .await
                .map_err(|e| e.to_string())?;
            groups
                .set_locked(&group, locked)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// Pending membership requests for a group.
    pub fn list_group_join_requests(
        &self,
        group_jid: String,
    ) -> Task<Result<Vec<JoinRequestView>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let group: Jid = group_jid
                .parse()
                .map_err(|_| "not a group address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let requests = live
                .client
                .groups()
                .get_membership_requests(&group)
                .await
                .map_err(|e| e.to_string())?;
            Ok(requests
                .into_iter()
                .map(|r| JoinRequestView {
                    jid: r.jid.to_string(),
                    request_time_secs: r.request_time,
                })
                .collect())
        })
    }

    /// Approve or reject one pending membership request.
    pub fn manage_group_join_request(
        &self,
        group_jid: String,
        participant_jid: String,
        approve: bool,
    ) -> Task<Result<(), String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let group: Jid = group_jid
                .parse()
                .map_err(|_| "not a group address".to_string())?;
            let participant = parse_participant(&participant_jid)?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let groups = live.client.groups();
            if approve {
                groups
                    .approve_membership_requests(&group, &[participant])
                    .await
                    .map_err(|e| e.to_string())?;
            } else {
                groups
                    .reject_membership_requests(&group, &[participant])
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        })
    }
}

/// One group in the participating set.
#[derive(Debug, Clone)]
pub struct GroupListEntry {
    pub jid: String,
    pub subject: String,
    pub participant_count: usize,
}

/// Full metadata for one group.
#[derive(Debug, Clone)]
pub struct GroupDetails {
    pub jid: String,
    pub subject: String,
    pub description: Option<String>,
    pub owner_jid: Option<String>,
    pub participant_count: usize,
    pub participants: Vec<GroupParticipantView>,
    pub announce_only: bool,
    pub locked: bool,
}

/// One member with its role.
#[derive(Debug, Clone)]
pub struct GroupParticipantView {
    pub jid: String,
    pub is_admin: bool,
    pub is_superadmin: bool,
}

/// What to do to a group participant.
#[derive(Debug, Clone, Copy)]
pub enum ParticipantChange {
    Add,
    Remove,
    Promote,
    Demote,
}

/// One pending membership request.
#[derive(Debug, Clone)]
pub struct JoinRequestView {
    pub jid: String,
    pub request_time_secs: Option<u64>,
}

/// What joining through an invite code produced.
#[derive(Debug, Clone)]
pub struct JoinGroupView {
    pub jid: String,
    pub pending_approval: bool,
}

/// A JID or a bare phone number, the way a script passes people.
fn parse_participant(raw: &str) -> Result<Jid, String> {
    if let Ok(jid) = raw.parse::<Jid>() {
        return Ok(jid);
    }
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() >= 8 {
        return Ok(Jid::pn(&digits));
    }
    Err(format!("not a JID or phone number: {raw}"))
}
