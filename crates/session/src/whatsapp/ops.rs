//! Operations a scriptable front end asks for beyond reads and sends:
//! polls, profiles, contacts, locations, status broadcasts, channels,
//! media fetch and history coverage.
//!
//! The shapes returned here are session-owned views — plain data with no
//! wire in them. The daemon maps each onto its protocol DTOs, which is
//! what keeps this crate from depending on the transport it serves.

use whatsapp_rust::wacore::proto_helpers::MessageExt as _;
use whatsapp_rust::wacore_binary::jid::{Jid, JidExt as _};
use whatsapp_rust::waproto::whatsapp as wa;

use super::WhatsAppClient;
use super::convert::stored_to_chat_message;
use crate::exec::Task;

/// How long a backfill waits for the phone's answer to land in the store.
///
/// The request returns as soon as it is sent; the rows arrive later, on the
/// history-sync path. Long enough for a cold phone to wake and answer, short
/// enough that an unreachable one costs a wait and not a hung command.
const HISTORY_SYNC_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// One poll, as its creation message describes it.
///
/// Tallies are not decrypted here: votes arrive encrypted to the creation's
/// secret and counting them is a separate pass. What listing answers is
/// what exists and what can be voted on.
#[derive(Debug, Clone)]
pub struct PollView {
    pub id: String,
    pub chat_jid: String,
    pub question: String,
    pub options: Vec<String>,
    pub selectable_count: u32,
}

/// A profile as the network describes it.
#[derive(Debug, Clone, Default)]
pub struct ProfileView {
    pub jid: String,
    pub name: Option<String>,
    pub about: Option<String>,
    pub picture_url: Option<String>,
    pub is_business: bool,
}

/// Whether a phone number is on WhatsApp.
#[derive(Debug, Clone)]
pub struct ContactCheckView {
    pub phone: String,
    pub is_registered: bool,
    pub jid: Option<String>,
}

/// One address-book row, labels included.
#[derive(Debug, Clone)]
pub struct ContactView {
    pub jid: String,
    pub name: Option<String>,
    pub push_name: Option<String>,
    pub phone: Option<String>,
    pub is_business: bool,
    pub alias: Option<String>,
    pub tags: Vec<String>,
}

/// An address-book row as the plain view the daemon maps onto its DTO.
fn contact_view_of(entry: oxidezap_chat_store::ContactEntry) -> ContactView {
    let jid = entry.jid.to_string();
    let phone = jid.split('@').next().map(str::to_string);
    let is_business = entry.business_name.is_some();
    ContactView {
        jid,
        name: entry.full_name.or(entry.first_name).or(entry.business_name),
        push_name: entry.push_name,
        phone,
        is_business,
        alias: entry.alias,
        tags: entry.tags,
    }
}

/// One subscribed channel.
#[derive(Debug, Clone)]
pub struct ChannelView {
    pub jid: String,
    pub name: String,
    pub description: Option<String>,
    pub subscriber_count: u64,
    pub picture_url: Option<String>,
}

/// How much history the store holds.
#[derive(Debug, Clone, Copy)]
pub struct HistoryCoverage {
    pub stored_count: u64,
    pub oldest_ms: Option<i64>,
    pub newest_ms: Option<i64>,
}

/// One backfilled media file, with the hash the daemon keys its cache by.
#[derive(Debug, Clone)]
pub struct BackfillFile {
    pub message_id: String,
    pub chat_jid: String,
    pub file_enc_sha256: Vec<u8>,
    pub bytes: Vec<u8>,
}

/// What a media backfill attempted and fetched.
#[derive(Debug, Clone)]
pub struct BackfillReport {
    pub requested: u64,
    pub files: Vec<BackfillFile>,
}

impl WhatsAppClient {
    /// Create a poll. Returns the server-assigned message id and timestamp.
    pub fn create_poll(
        &self,
        to: String,
        question: String,
        options: Vec<String>,
        selectable_count: u32,
    ) -> Task<Result<(String, i64), String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat: Jid = to.parse().map_err(|_| "not a chat address".to_string())?;
            if question.is_empty() {
                return Err("poll question must not be empty".to_string());
            }
            if options.len() < 2 {
                return Err("a poll needs at least two options".to_string());
            }
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let (result, _secret) = live
                .client
                .polls()
                .create(&chat, &question, &options, selectable_count.max(1))
                .await
                .map_err(|e| e.to_string())?;
            Ok((
                result.message_id.clone(),
                whatsapp_rust::wacore::time::now_millis(),
            ))
        })
    }

    /// Vote on a poll by option index.
    ///
    /// The names, the secret and the creator behind those indexes come from
    /// the stored creation message: a vote encrypts the option hashes to
    /// the creation's secret, and none of the three is guessable.
    pub fn vote_poll(
        &self,
        chat_jid: String,
        poll_id: String,
        selected_option_indices: Vec<u32>,
    ) -> Task<Result<(), String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat: Jid = chat_jid
                .parse()
                .map_err(|_| "not a chat address".to_string())?;
            if selected_option_indices.is_empty() {
                return Err("no options selected".to_string());
            }
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let stored = live
                .chat_store
                .message(&chat, &poll_id)
                .await
                .map_err(|e| format!("database query failed: {e}"))?
                .ok_or_else(|| "poll message not found".to_string())?;
            let proto = stored
                .message
                .as_deref()
                .ok_or_else(|| "that message has no poll content".to_string())?;
            let base = proto.get_base_message();
            let creation =
                poll_creation_of(base).ok_or_else(|| "that message is not a poll".to_string())?;
            let mut names = Vec::with_capacity(selected_option_indices.len());
            for index in &selected_option_indices {
                let option = creation
                    .options
                    .get(*index as usize)
                    .and_then(|o| o.option_name.clone())
                    .ok_or_else(|| format!("poll has no option {index}"))?;
                names.push(option);
            }
            let secret = base
                .message_context_info
                .as_option()
                .and_then(|info| info.message_secret.clone())
                .ok_or_else(|| "the poll's secret was not stored".to_string())?;
            live.client
                .polls()
                .vote(&chat, &poll_id, &stored.sender_jid, &secret, &names)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }

    /// Polls as their creation messages describe them, newest first.
    pub fn list_polls(
        &self,
        chat_jid: Option<String>,
        limit: i64,
    ) -> Task<Result<Vec<PollView>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat = chat_jid
                .map(|j| {
                    j.parse::<Jid>()
                        .map_err(|_| "not a chat address".to_string())
                })
                .transpose()?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let rows = live
                .chat_store
                .poll_messages(chat.as_ref(), limit.clamp(1, 100))
                .await
                .map_err(|e| format!("database query failed: {e}"))?;
            Ok(rows.into_iter().filter_map(poll_view_of).collect())
        })
    }

    /// One poll by its message id.
    pub fn get_poll(
        &self,
        chat_jid: String,
        poll_id: String,
    ) -> Task<Result<Option<PollView>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat: Jid = chat_jid
                .parse()
                .map_err(|_| "not a chat address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let stored = live
                .chat_store
                .message(&chat, &poll_id)
                .await
                .map_err(|e| format!("database query failed: {e}"))?;
            Ok(stored.and_then(poll_view_of))
        })
    }

    /// Send a location pin.
    pub fn send_location(
        &self,
        to: String,
        latitude: f64,
        longitude: f64,
        name: Option<String>,
    ) -> Task<Result<(String, i64), String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat: Jid = to.parse().map_err(|_| "not a chat address".to_string())?;
            if !latitude.is_finite() || !longitude.is_finite() {
                return Err("invalid coordinates".to_string());
            }
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let message = wa::Message {
                location_message: whatsapp_rust::buffa::MessageField::some(
                    wa::message::LocationMessage {
                        degrees_latitude: Some(latitude),
                        degrees_longitude: Some(longitude),
                        name,
                        ..Default::default()
                    },
                ),
                ..Default::default()
            };
            let client = &live.client;
            let msg_id = client.generate_message_id();
            super::record_outgoing(&live.chat_store, &chat, &msg_id, &message);
            let options = whatsapp_rust::SendOptions::default().with_message_id(msg_id.clone());
            match client
                .send_message_with_options(chat.clone(), message, options)
                .await
            {
                Ok(_) => Ok((msg_id, whatsapp_rust::wacore::time::now_millis())),
                Err(e) => {
                    super::mark_send_failed(&live.chat_store, &chat, &msg_id);
                    Err(e.to_string())
                }
            }
        })
    }

    /// Publish a text status broadcast.
    pub fn send_status(&self, text: String) -> Task<Result<(String, i64), String>> {
        use whatsapp_rust::features::{StatusPrivacySetting, StatusSendOptions};

        let session = self.session.clone();
        self.exec.spawn(async move {
            if text.is_empty() {
                return Err("status text must not be empty".to_string());
            }
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let result = live
                .client
                .status()
                .send_text(
                    &text,
                    0xFF1E_6E4F,
                    wa::message::extended_text_message::FontType::System,
                    &[],
                    StatusSendOptions {
                        privacy: StatusPrivacySetting::Contacts,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
            Ok((
                result.message_id.clone(),
                whatsapp_rust::wacore::time::now_millis(),
            ))
        })
    }

    /// A profile as the network describes it: about, picture and business
    /// name. `None` asks about this account itself.
    pub fn get_profile(&self, jid: Option<String>) -> Task<Result<ProfileView, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let target: Jid = match jid {
                Some(j) => j.parse().map_err(|_| "not a user address".to_string())?,
                None => live
                    .client
                    .pn()
                    .ok_or_else(|| "not logged in".to_string())?,
            };
            let client = &live.client;
            let info = client
                .contacts()
                .get_user_info(std::slice::from_ref(&target))
                .await
                .map_err(|e| e.to_string())?;
            let about = info.values().next().and_then(|u| u.status.clone());
            let is_business = info.values().next().is_some_and(|u| u.is_business);
            let picture_url = client
                .contacts()
                .get_profile_picture(&target, false)
                .await
                .ok()
                .flatten()
                .map(|p| p.url);
            let name = live
                .chat_store
                .contact(&target)
                .await
                .ok()
                .flatten()
                .and_then(|c| c.full_name.or(c.push_name).or(c.business_name));
            Ok(ProfileView {
                jid: target.to_string(),
                name,
                about,
                picture_url,
                is_business,
            })
        })
    }

    /// A business profile: verified name, about and picture.
    pub fn get_business_profile(&self, jid: String) -> Task<Result<ProfileView, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let target: Jid = jid.parse().map_err(|_| "not a user address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let client = &live.client;
            let info = client
                .contacts()
                .get_user_info(std::slice::from_ref(&target))
                .await
                .map_err(|e| e.to_string())?;
            let user = info
                .values()
                .next()
                .ok_or_else(|| "no profile returned".to_string())?;
            let name = user.verified_name.as_ref().and_then(|v| v.name.clone());
            let picture_url = client
                .contacts()
                .get_profile_picture(&target, false)
                .await
                .ok()
                .flatten()
                .map(|p| p.url);
            Ok(ProfileView {
                jid: target.to_string(),
                name,
                about: user.status.clone(),
                picture_url,
                is_business: user.is_business,
            })
        })
    }

    /// Set the "recado" (about) text.
    pub fn set_profile_about(&self, about: String) -> Task<Result<(), String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            live.client
                .profile()
                .set_status_text(&about)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// Set the push display name.
    pub fn set_profile_name(&self, name: String) -> Task<Result<(), String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            if name.is_empty() {
                return Err("profile name must not be empty".to_string());
            }
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            live.client
                .profile()
                .set_push_name(&name)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// Set the profile picture from a daemon-local image file.
    pub fn set_profile_picture_from_path(&self, file_path: String) -> Task<Result<(), String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let path = std::path::PathBuf::from(&file_path);
            let data = crate::exec::unblock(move || std::fs::read(&path))
                .await
                .map_err(|e| format!("the update could not start: {e}"))?
                .map_err(|e| format!("could not read {file_path}: {e}"))?;
            if data.is_empty() {
                return Err(format!("{file_path} is empty"));
            }
            live.client
                .profile()
                .set_profile_picture(data)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }

    /// Remove the profile picture.
    pub fn remove_profile_picture(&self) -> Task<Result<(), String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            live.client
                .profile()
                .remove_profile_picture()
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }

    /// Check live whether a phone number is registered on WhatsApp.
    pub fn check_contact(&self, phone: String) -> Task<Result<ContactCheckView, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
            if digits.len() < 8 {
                return Err(format!("not a phone number: {phone}"));
            }
            let jid = Jid::pn(&digits);
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let answers = live
                .client
                .contacts()
                .is_on_whatsapp(std::slice::from_ref(&jid))
                .await
                .map_err(|e| e.to_string())?;
            let first = answers.into_iter().next();
            Ok(ContactCheckView {
                phone: digits,
                is_registered: first.as_ref().is_some_and(|r| r.is_registered),
                jid: first.map(|r| r.jid.to_string()),
            })
        })
    }

    /// Set (or clear, with `None`) a contact's device-local alias,
    /// returning the updated row.
    pub fn set_contact_alias(
        &self,
        jid: String,
        alias: Option<String>,
    ) -> Task<Result<Option<ContactView>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let target: Jid = jid.parse().map_err(|_| "not a user address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            live.chat_store
                .set_contact_alias(&target, alias)
                .await
                .map_err(|e| format!("database query failed: {e}"))?;
            live.chat_store
                .contact(&target)
                .await
                .map(|entry| entry.map(contact_view_of))
                .map_err(|e| format!("database query failed: {e}"))
        })
    }

    /// Add a device-local tag to a contact, returning the updated row.
    pub fn tag_contact(
        &self,
        jid: String,
        tag: String,
    ) -> Task<Result<Option<ContactView>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let target: Jid = jid.parse().map_err(|_| "not a user address".to_string())?;
            if tag.is_empty() {
                return Err("tag must not be empty".to_string());
            }
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            live.chat_store
                .tag_contact(&target, tag)
                .await
                .map_err(|e| format!("database query failed: {e}"))?;
            live.chat_store
                .contact(&target)
                .await
                .map(|entry| entry.map(contact_view_of))
                .map_err(|e| format!("database query failed: {e}"))
        })
    }

    /// Remove a device-local tag from a contact, returning the updated row.
    pub fn untag_contact(
        &self,
        jid: String,
        tag: String,
    ) -> Task<Result<Option<ContactView>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let target: Jid = jid.parse().map_err(|_| "not a user address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            live.chat_store
                .untag_contact(&target, &tag)
                .await
                .map_err(|e| format!("database query failed: {e}"))?;
            live.chat_store
                .contact(&target)
                .await
                .map(|entry| entry.map(contact_view_of))
                .map_err(|e| format!("database query failed: {e}"))
        })
    }

    /// Contacts matching a query, labels included.
    pub fn list_contact_views(
        &self,
        query: Option<String>,
        limit: i64,
    ) -> Task<Result<Vec<ContactView>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let entries = live
                .chat_store
                .contacts(query, limit.clamp(1, 200))
                .await
                .map_err(|e| format!("loading contacts failed: {e}"))?;
            Ok(entries.into_iter().map(contact_view_of).collect())
        })
    }

    /// Refresh contacts from the network, then read back the refreshed rows:
    /// the one asked about, or the first page of the book.
    pub fn refresh_contacts_and_read(
        &self,
        jid: Option<String>,
    ) -> Task<Result<Vec<ContactView>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            Self::refresh_into(&session, jid.clone()).await?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            match jid {
                Some(j) => {
                    let target: Jid = j.parse().map_err(|_| "not a user address".to_string())?;
                    let entry = live
                        .chat_store
                        .contact(&target)
                        .await
                        .map_err(|e| format!("database query failed: {e}"))?;
                    Ok(entry.into_iter().map(contact_view_of).collect())
                }
                None => {
                    let entries = live
                        .chat_store
                        .contacts(None, 200)
                        .await
                        .map_err(|e| format!("loading contacts failed: {e}"))?;
                    Ok(entries.into_iter().map(contact_view_of).collect())
                }
            }
        })
    }

    /// The network half of a contact refresh, shared by the counter and the
    /// read-back.
    async fn refresh_into(
        session: &super::SessionSlot,
        jid: Option<String>,
    ) -> Result<usize, String> {
        let targets: Vec<Jid> = match jid {
            Some(j) => vec![j.parse().map_err(|_| "not a user address".to_string())?],
            None => {
                let Some(live) = session.lock().await.clone() else {
                    return Err("no session yet".to_string());
                };
                live.chat_store
                    .contacts(None, 10_000)
                    .await
                    .map_err(|e| format!("database query failed: {e}"))?
                    .into_iter()
                    .map(|c| c.jid)
                    .collect()
            }
        };
        let Some(live) = session.lock().await.clone() else {
            return Err("no session yet".to_string());
        };
        let mut refreshed = 0;
        for chunk in targets.chunks(50) {
            let info = live
                .client
                .contacts()
                .get_user_info(chunk)
                .await
                .map_err(|e| e.to_string())?;
            refreshed += info.len();
        }
        Ok(refreshed)
    }

    /// One contact row by JID.
    pub fn get_contact(&self, jid: String) -> Task<Result<Option<ContactView>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let target: Jid = jid.parse().map_err(|_| "not a user address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            live.chat_store
                .contact(&target)
                .await
                .map(|entry| entry.map(contact_view_of))
                .map_err(|e| format!("database query failed: {e}"))
        })
    }

    /// Refresh what the network knows about contacts: user info for every
    /// stored contact, in bounded batches, or just one when asked.
    ///
    /// Returns how many contacts answered. The library persists the LID
    /// mappings behind it, so the address book converges as a side effect.
    pub fn refresh_contacts(&self, jid: Option<String>) -> Task<Result<usize, String>> {
        let session = self.session.clone();
        self.exec
            .spawn(async move { Self::refresh_into(&session, jid).await })
    }

    /// Subscribed channels.
    pub fn list_channels(&self) -> Task<Result<Vec<ChannelView>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let subscribed = live
                .client
                .newsletter()
                .list_subscribed()
                .await
                .map_err(|e| e.to_string())?;
            Ok(subscribed.into_iter().map(channel_view_of).collect())
        })
    }

    /// One channel by JID.
    pub fn get_channel(&self, channel_jid: String) -> Task<Result<ChannelView, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let channel: Jid = channel_jid
                .parse()
                .map_err(|_| "not a channel address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let meta = live
                .client
                .newsletter()
                .get_metadata(&channel)
                .await
                .map_err(|e| e.to_string())?;
            Ok(channel_view_of(meta))
        })
    }

    /// Follow a channel.
    pub fn join_channel(&self, channel_jid: String) -> Task<Result<ChannelView, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let channel: Jid = channel_jid
                .parse()
                .map_err(|_| "not a channel address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let meta = live
                .client
                .newsletter()
                .join(&channel)
                .await
                .map_err(|e| e.to_string())?;
            Ok(channel_view_of(meta))
        })
    }

    /// Unfollow a channel.
    pub fn leave_channel(&self, channel_jid: String) -> Task<Result<(), String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let channel: Jid = channel_jid
                .parse()
                .map_err(|_| "not a channel address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            live.client
                .newsletter()
                .leave(&channel)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// Download one message's media bytes.
    pub fn download_media_by_id(
        &self,
        chat_jid: String,
        message_id: String,
    ) -> Task<Result<Vec<u8>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat: Jid = chat_jid
                .parse()
                .map_err(|_| "not a chat address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let stored = live
                .chat_store
                .message(&chat, &message_id)
                .await
                .map_err(|e| format!("database query failed: {e}"))?
                .ok_or_else(|| "message not found".to_string())?;
            let downloadable = stored_to_chat_message(stored)
                .media
                .and_then(|media| media.downloadable)
                .ok_or_else(|| "that message carries no downloadable media".to_string())?;
            live.client
                .download(&downloadable)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// Ask the phone to re-upload expired media, returning the fresh path.
    pub fn retry_media(
        &self,
        chat_jid: String,
        message_id: String,
    ) -> Task<Result<String, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat: Jid = chat_jid
                .parse()
                .map_err(|_| "not a chat address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let stored = live
                .chat_store
                .message(&chat, &message_id)
                .await
                .map_err(|e| format!("database query failed: {e}"))?
                .ok_or_else(|| "message not found".to_string())?;
            let from_me = stored.from_me;
            let sender = stored.sender_jid.clone();
            let downloadable = stored_to_chat_message(stored)
                .media
                .and_then(|media| media.downloadable)
                .ok_or_else(|| "that message carries no downloadable media".to_string())?;
            let participant = (!from_me && chat.is_group()).then_some(sender);
            let request = whatsapp_rust::features::MediaReuploadRequest {
                msg_id: &message_id,
                chat_jid: &chat,
                media_key: &downloadable.media_key,
                is_from_me: from_me,
                participant: participant.as_ref(),
            };
            live.client
                .media_reupload()
                .request(&request)
                .await
                .map(|answer| format!("{answer:?}"))
                .map_err(|e| e.to_string())
        })
    }

    /// Starred messages, newest first, hydrated like search hits.
    ///
    /// Each message rides with its chat: starred spans conversations, and
    /// the row is where the reader jumps to.
    pub fn list_starred(
        &self,
        limit: i64,
    ) -> Task<Result<Vec<(String, oxidezap_core::ChatMessage)>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let rows = live
                .chat_store
                .starred_messages(limit.clamp(1, 200))
                .await
                .map_err(|e| format!("database query failed: {e}"))?;
            let chats: Vec<String> = rows.iter().map(|s| s.chat_jid.to_string()).collect();
            let mut messages: Vec<oxidezap_core::ChatMessage> =
                rows.into_iter().map(stored_to_chat_message).collect();
            Self::hydrate_sender_names(
                &live.chat_store,
                &live.client,
                &mut messages,
                &live.names,
                false,
            )
            .await;
            Ok(chats.into_iter().zip(messages).collect())
        })
    }

    /// How much history the store holds, per chat or account-wide.
    pub fn history_coverage(
        &self,
        chat_jid: Option<String>,
    ) -> Task<Result<HistoryCoverage, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat = chat_jid
                .map(|j| {
                    j.parse::<Jid>()
                        .map_err(|_| "not a chat address".to_string())
                })
                .transpose()?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let coverage = live
                .chat_store
                .message_coverage(chat.as_ref())
                .await
                .map_err(|e| format!("database query failed: {e}"))?;
            Ok(HistoryCoverage {
                stored_count: coverage.stored_count,
                oldest_ms: coverage.oldest_ms,
                newest_ms: coverage.newest_ms,
            })
        })
    }

    /// Backfill history: warm the chat page and report the coverage.
    ///
    /// Asks the primary phone first, through PDO, for whatever came before the
    /// oldest row the store holds — the library's `fetch_message_history`,
    /// which is the on-demand sync WA Web itself uses. What arrives lands on
    /// the normal history-sync path, so the local re-read that follows drains
    /// it. A phone that refuses the request is not fatal: the local backfill is
    /// still worth having, and what it warms is what already exists here.
    pub fn history_backfill(
        &self,
        chat_jid: String,
        count: i64,
    ) -> Task<Result<HistoryCoverage, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat: Jid = chat_jid
                .parse()
                .map_err(|_| "not a chat address".to_string())?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let oldest = live
                .chat_store
                .oldest_message(&chat)
                .await
                .map_err(|e| format!("database query failed: {e}"))?;
            if let Some(oldest) = oldest {
                let from_me = oldest.from_me;
                if let Err(e) = live
                    .client
                    .fetch_message_history(
                        &chat,
                        &oldest.id,
                        from_me,
                        oldest.timestamp.timestamp_millis(),
                        count.clamp(1, 500) as i32,
                    )
                    .await
                {
                    // Nothing was asked, so nothing is coming: waiting for a
                    // history sync would spend the whole budget on an answer
                    // that will never arrive. The local backfill below still
                    // runs.
                    log::warn!("the phone was not asked for older history: {e}");
                } else {
                    // The phone answers asynchronously: the messages arrive as
                    // a history-sync notification the store materializes, not
                    // as the return of the request above. Reading coverage
                    // straight away would report what was here before the ask.
                    // Bounded, so a phone that never answers costs a wait and
                    // not a hang.
                    let before = oldest.timestamp.timestamp_millis();
                    let deadline = wacore::time::Instant::now() + HISTORY_SYNC_WAIT;
                    loop {
                        match live.chat_store.oldest_message(&chat).await {
                            Ok(Some(now)) if now.timestamp.timestamp_millis() < before => break,
                            Ok(_) => {}
                            Err(e) => {
                                log::warn!("could not read the chat while waiting on history: {e}");
                                break;
                            }
                        }
                        if wacore::time::Instant::now() >= deadline {
                            break;
                        }
                        crate::exec::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }
            let warmed = Self::message_page(
                &live.chat_store,
                &live.client,
                &live.names,
                chat.to_string(),
                None,
                count.clamp(1, 500),
            )
            .await
            .map_err(|e| e.to_string())?;
            drop(warmed);
            let coverage = live
                .chat_store
                .message_coverage(Some(&chat))
                .await
                .map_err(|e| format!("database query failed: {e}"))?;
            Ok(HistoryCoverage {
                stored_count: coverage.stored_count,
                oldest_ms: coverage.oldest_ms,
                newest_ms: coverage.newest_ms,
            })
        })
    }

    /// Drop the stored payload of revoked messages, keeping tombstones.
    pub fn purge_messages(&self, chat_jid: Option<String>) -> Task<Result<u64, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat = chat_jid
                .map(|j| {
                    j.parse::<Jid>()
                        .map_err(|_| "not a chat address".to_string())
                })
                .transpose()?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            live.chat_store
                .purge_revoked_payload(chat.as_ref())
                .await
                .map_err(|e| format!("database query failed: {e}"))
        })
    }

    /// Delete empty chat rows. Returns how many were removed.
    pub fn cleanup_chats(&self) -> Task<Result<u64, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            live.chat_store
                .cleanup_empty_chats()
                .await
                .map_err(|e| format!("database query failed: {e}"))
        })
    }

    /// Bulk-fetch media for the backfill candidates: every message carrying
    /// media, with its bytes and the hash the daemon keys its cache by.
    ///
    /// Failures are per-message, not fatal: a backfill repairs what it can
    /// and reports the rest by omission, which is what the daemon counts
    /// against the candidates. The fetch is bounded by the limit, like
    /// every other listing here.
    pub fn backfill_media(
        &self,
        chat_jid: Option<String>,
        limit: i64,
    ) -> Task<Result<BackfillReport, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat = chat_jid
                .map(|j| {
                    j.parse::<Jid>()
                        .map_err(|_| "not a chat address".to_string())
                })
                .transpose()?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let rows = live
                .chat_store
                .pending_media_messages(chat.as_ref(), limit.clamp(1, 200))
                .await
                .map_err(|e| format!("database query failed: {e}"))?;
            let mut report = BackfillReport {
                requested: 0,
                files: Vec::new(),
            };
            for stored in rows {
                let chat_jid = stored.chat_jid.to_string();
                let message_id = stored.id.clone();
                let Some(downloadable) = stored_to_chat_message(stored)
                    .media
                    .and_then(|media| media.downloadable)
                else {
                    continue;
                };
                report.requested += 1;
                match live.client.download(&downloadable).await {
                    Ok(bytes) => report.files.push(BackfillFile {
                        message_id,
                        chat_jid,
                        file_enc_sha256: downloadable.file_enc_sha256.clone(),
                        bytes,
                    }),
                    Err(e) => {
                        log::warn!("a backfill download failed: {e}");
                    }
                }
            }
            Ok(report)
        })
    }

    /// Messages carrying media, naming a media backfill's candidates.
    ///
    /// Each message rides with its chat, like starred: the backfill
    /// downloads by (chat, id).
    pub fn pending_media(
        &self,
        chat_jid: Option<String>,
        limit: i64,
    ) -> Task<Result<Vec<(String, oxidezap_core::ChatMessage)>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let chat = chat_jid
                .map(|j| {
                    j.parse::<Jid>()
                        .map_err(|_| "not a chat address".to_string())
                })
                .transpose()?;
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let rows = live
                .chat_store
                .pending_media_messages(chat.as_ref(), limit.clamp(1, 200))
                .await
                .map_err(|e| format!("database query failed: {e}"))?;
            let chats: Vec<String> = rows.iter().map(|s| s.chat_jid.to_string()).collect();
            let messages: Vec<oxidezap_core::ChatMessage> =
                rows.into_iter().map(stored_to_chat_message).collect();
            Ok(chats.into_iter().zip(messages).collect())
        })
    }
}

/// The creation message behind a stored row, either protocol version.
fn poll_creation_of(message: &wa::Message) -> Option<&wa::message::PollCreationMessage> {
    message
        .poll_creation_message_v3
        .as_option()
        .or_else(|| message.poll_creation_message.as_option())
}

/// A stored row as the poll its creation message describes.
fn poll_view_of(stored: oxidezap_chat_store::StoredMessage) -> Option<PollView> {
    let proto = stored.message.as_deref()?;
    let creation = poll_creation_of(proto.get_base_message())?;
    Some(PollView {
        id: stored.id,
        chat_jid: stored.chat_jid.to_string(),
        question: creation.name.clone().unwrap_or_default(),
        options: creation
            .options
            .iter()
            .filter_map(|o| o.option_name.clone())
            .collect(),
        selectable_count: creation.selectable_options_count.unwrap_or(1),
    })
}

/// A subscribed channel as a plain view.
fn channel_view_of(meta: whatsapp_rust::features::NewsletterMetadata) -> ChannelView {
    ChannelView {
        jid: meta.jid.to_string(),
        name: meta.name,
        description: meta.description,
        subscriber_count: meta.subscriber_count,
        picture_url: meta.picture_url,
    }
}
