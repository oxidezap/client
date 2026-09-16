//! Pages, the cursors that continue them, and where a read stops.
//!
//! Everything here is about a *position* in what the store holds rather than
//! about the rows at it: the token a front end hands back to ask for the page
//! after the one it has, the clamp that says how large a page may be, and the
//! range of message keys a read receipt covers. The tokens are written and
//! read on this side alone — see [`Page`] — so both halves of each one sit
//! beside each other and what a page is ordered by has one file to change.

use std::sync::Arc;

use log::warn;
use oxidezap_chat_store::{ChatEntry, ChatStore};
use whatsapp_rust::client::Client;
use whatsapp_rust::wacore_binary::jid::{Jid, JidExt, observe_str};
use whatsapp_rust::waproto::whatsapp as wa;

use oxidezap_core::{Chat, ChatMessage};

use super::WhatsAppClient;
use super::convert::{mark_unread_tail, stored_to_chat_message};
use crate::exec::Task;
use crate::names::NameBook;

/// One page of something, and where to continue.
///
/// `next` is a token this crate writes and this crate reads. Nothing outside
/// it may parse one: what a page is ordered by is a fact about the store's
/// indexes, and a caller that took the token apart would be a second
/// implementation of that order. `None` is the end of the list — there is no
/// position after the last row, so absence is the only honest way to say so.
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
}

/// The cursor for continuing a conversation before `message`.
pub(super) fn message_cursor(message: &oxidezap_chat_store::StoredMessage) -> String {
    let cursor = oxidezap_chat_store::MessageCursor::from(message);
    format!("m1:{}:{}", cursor.timestamp_ms, cursor.seq)
}

pub(super) fn parse_message_cursor(token: &str) -> Option<oxidezap_chat_store::MessageCursor> {
    let mut parts = token.strip_prefix("m1:")?.split(':');
    Some(oxidezap_chat_store::MessageCursor {
        timestamp_ms: parts.next()?.parse().ok()?,
        seq: parts.next()?.parse().ok()?,
    })
}

/// The cursor for continuing the chat list after `entry`.
///
/// The JID goes last and is not split on, because a device address carries a
/// colon of its own (`5599…:57`).
pub(super) fn chat_cursor(entry: &ChatEntry) -> String {
    let cursor = oxidezap_chat_store::ChatCursor::from(entry);
    let pinned = cursor
        .pinned_at_ms
        .map_or_else(|| "-".to_string(), |t| t.to_string());
    format!("c1:{pinned}:{}:{}", cursor.last_message_ts, cursor.jid)
}

pub(super) fn parse_chat_cursor(token: &str) -> Option<oxidezap_chat_store::ChatCursor> {
    let mut parts = token.strip_prefix("c1:")?.splitn(3, ':');
    // An unreadable pin is an unreadable cursor, not an unpinned chat: read as
    // `None` it is a valid position in the wrong half of the order, and the
    // next page silently skips or repeats conversations.
    let pinned_at_ms = match parts.next()? {
        "-" => None,
        pinned => Some(pinned.parse().ok()?),
    };
    Some(oxidezap_chat_store::ChatCursor {
        pinned_at_ms,
        last_message_ts: parts.next()?.parse().ok()?,
        jid: parts.next()?.to_string(),
    })
}

pub type ReadBoundary = (i64, Vec<(String, bool, Option<String>)>);

pub(super) fn participant_keyed_chat(jid: &Jid) -> bool {
    jid.is_group() || jid.is_broadcast_list() || jid.is_status_broadcast()
}

/// Whether a stored message's class carries an attachment.
///
/// The search filter `has_media` reads, kept here beside the message class so
/// the two move together.
fn carries_media(kind: &oxidezap_chat_store::MessageKind) -> bool {
    use oxidezap_chat_store::MessageKind as K;
    matches!(
        kind,
        K::Image | K::Video | K::VideoNote | K::Audio | K::VoiceNote | K::Sticker | K::Document
    )
}

impl WhatsAppClient {
    /// One page of a conversation, for a front end that asked for one.
    ///
    /// The number WhatsApp Web's own on-demand history request uses
    /// (`history_sync_on_demand_message_count`), and near enough to a screenful
    /// of bubbles that scrolling back asks again rather than stalling.
    pub const MESSAGE_PAGE: i64 = 50;
    /// One page of the chat list.
    ///
    /// WA Web's `web_init_chat_batch_size`, and the same number the list has
    /// always loaded at once.
    pub const CHAT_PAGE: i64 = 100;

    /// One page of a chat's messages, older than `before`.
    ///
    /// The read a front end makes when it opens a conversation and again when
    /// it scrolls back through one. Hydrated exactly as the attach load
    /// hydrates its rows — reactions, sender names — because a bubble drawn
    /// from a page and the same bubble drawn from a load must say the same
    /// thing.
    ///
    /// The cursor is this side's to write and to read: see [`Page`].
    pub fn load_messages(
        &self,
        jid: String,
        before: Option<String>,
        limit: i64,
    ) -> Task<Result<Page<ChatMessage>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            // One answer to one question. Three slots asked in turn could each
            // be answered differently — a store without the book that has to
            // name the people in it — and the three answers came from three
            // moments, any of which a teardown could fall between.
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            Self::message_page(
                &live.chat_store,
                &live.client,
                &live.names,
                jid,
                before,
                limit,
            )
            .await
        })
    }

    /// One page of a chat's messages *after* a cursor, oldest first.
    ///
    /// The forward twin of [`Self::load_messages`], for a caller that holds a
    /// row and wants what came later. A page shorter than it asked for is the
    /// end of what the store holds.
    pub fn load_messages_after(
        &self,
        jid: String,
        after: String,
        limit: i64,
    ) -> Task<Result<Page<ChatMessage>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            Self::message_page_after(
                &live.chat_store,
                &live.client,
                &live.names,
                jid,
                after,
                limit,
            )
            .await
        })
    }

    /// Full-text search over message history with optional chat filter.
    ///
    /// `has_media` keeps only rows whose content class carries an attachment,
    /// which is the one filter the CLI promises and the store can answer
    /// without a second query: the row's `kind` is already materialized.
    pub fn search_messages(
        &self,
        query: String,
        chat_jid: Option<String>,
        has_media: bool,
        limit: i64,
    ) -> Task<Result<Vec<ChatMessage>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let parsed_chat = if let Some(j) = chat_jid {
                Some(
                    j.parse::<Jid>()
                        .map_err(|_| "invalid chat jid".to_string())?,
                )
            } else {
                None
            };
            let hits = if let Some(ref chat) = parsed_chat {
                live.chat_store
                    .search_messages_in_chat(chat, &query, limit.clamp(1, 100))
                    .await
                    .map_err(|e| format!("search failed: {e}"))?
            } else {
                live.chat_store
                    .search_messages(&query, limit.clamp(1, 100))
                    .await
                    .map_err(|e| format!("search failed: {e}"))?
            };
            let hits = if has_media {
                hits.into_iter()
                    .filter(|m| carries_media(&m.kind))
                    .collect()
            } else {
                hits
            };
            let mut messages: Vec<ChatMessage> =
                hits.into_iter().map(stored_to_chat_message).collect();
            Self::hydrate_sender_names(
                &live.chat_store,
                &live.client,
                &mut messages,
                &live.names,
                false,
            )
            .await;
            Ok(messages)
        })
    }

    /// Fetch a single message by ID.
    pub fn get_message(
        &self,
        chat_jid: String,
        message_id: String,
    ) -> Task<Result<Option<ChatMessage>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let chat = chat_jid
                .parse::<Jid>()
                .map_err(|_| "invalid chat jid".to_string())?;
            let stored = live
                .chat_store
                .message(&chat, &message_id)
                .await
                .map_err(|e| format!("database query failed: {e}"))?;
            if let Some(s) = stored {
                let mut msgs = vec![stored_to_chat_message(s)];
                Self::hydrate_sender_names(
                    &live.chat_store,
                    &live.client,
                    &mut msgs,
                    &live.names,
                    false,
                )
                .await;
                Ok(msgs.pop())
            } else {
                Ok(None)
            }
        })
    }

    /// Fetch context messages around a target message ID.
    pub fn get_message_context(
        &self,
        chat_jid: String,
        message_id: String,
        limit: usize,
    ) -> Task<Result<Vec<ChatMessage>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let chat = chat_jid
                .parse::<Jid>()
                .map_err(|_| "invalid chat jid".to_string())?;
            let target = live
                .chat_store
                .message(&chat, &message_id)
                .await
                .map_err(|e| format!("database query failed: {e}"))?;
            let Some(target) = target else {
                return Err("message not found".to_string());
            };
            let before_cursor = oxidezap_chat_store::MessageCursor::from(&target);
            let before_messages = live
                .chat_store
                .messages(&chat, Some(before_cursor), (limit / 2).max(1) as i64)
                .await
                .map_err(|e| format!("query failed: {e}"))?;
            let mut result = Vec::with_capacity(before_messages.len() + 1);
            for m in before_messages.into_iter().rev() {
                result.push(stored_to_chat_message(m));
            }
            result.push(stored_to_chat_message(target));
            Self::hydrate_sender_names(
                &live.chat_store,
                &live.client,
                &mut result,
                &live.names,
                false,
            )
            .await;
            Ok(result)
        })
    }

    /// Fetch one chat by JID, hydrated like a chat-list row.
    pub fn get_chat(&self, jid: String) -> Task<Result<Option<Chat>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let chat: Jid = jid.parse().map_err(|_| "not a chat address".to_string())?;
            let entry = live
                .chat_store
                .chat(&chat)
                .await
                .map_err(|e| format!("database query failed: {e}"))?;
            match entry {
                None => Ok(None),
                Some(entry) => {
                    let mut chats = Self::hydrate_entries(
                        &live.chat_store,
                        &live.client,
                        &live.names,
                        vec![entry],
                        Self::attach_page,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    Ok(chats.pop())
                }
            }
        })
    }

    /// Fetch contacts from the local contact store.
    pub fn load_contacts(
        &self,
        query: Option<String>,
        limit: i64,
    ) -> Task<Result<Vec<oxidezap_chat_store::ContactEntry>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            live.chat_store
                .contacts(query, limit.clamp(1, 200))
                .await
                .map_err(|e| format!("loading contacts failed: {e}"))
        })
    }

    pub(super) async fn message_page(
        store: &Arc<ChatStore>,
        client: &Arc<Client>,
        names: &NameBook,
        jid: String,
        before: Option<String>,
        limit: i64,
    ) -> Result<Page<ChatMessage>, String> {
        let chat: Jid = jid.parse().map_err(|_| "not a chat address".to_string())?;
        let before = before
            .map(|cursor| parse_message_cursor(&cursor).ok_or("unreadable cursor".to_string()))
            .transpose()?;

        let limit = limit.clamp(1, Self::MESSAGE_PAGE);
        // The page and how much of the unread tail it owes, out of one
        // snapshot: asked separately, a message committed between the two
        // raises the counter without appearing in the page, and the tail then
        // reaches a row further back than the page justifies — one already
        // read, advertised as owing a receipt.
        let (mut page, unread) = store
            .page_with_unread(&chat, before, limit)
            .await
            .map_err(|e| e.to_string())?;
        // A page shorter than it asked for is the start of the
        // conversation: there is nothing older to name a cursor with.
        let next = ((page.len() as i64) == limit)
            .then(|| page.last().map(message_cursor))
            .flatten();
        page.reverse(); // the store returns newest-first; a timeline is drawn the other way
        let messages = Self::hydrate_message_page(store, client, names, &chat, page, unread).await;
        Ok(Page {
            items: messages,
            next,
        })
    }

    /// One page of a chat's messages *after* a cursor, oldest first.
    ///
    /// The forward twin of [`Self::message_page`], hydrated through the same
    /// function so a bubble read forwards and the same bubble read backwards
    /// say the same thing. `next` is the last row's cursor, or `None` when the
    /// store had nothing left to hand over.
    pub(super) async fn message_page_after(
        store: &Arc<ChatStore>,
        client: &Arc<Client>,
        names: &NameBook,
        jid: String,
        after: String,
        limit: i64,
    ) -> Result<Page<ChatMessage>, String> {
        let chat: Jid = jid.parse().map_err(|_| "not a chat address".to_string())?;
        let after = parse_message_cursor(&after).ok_or_else(|| "unreadable cursor".to_string())?;

        let limit = limit.clamp(1, Self::MESSAGE_PAGE);
        let page = store
            .messages_after(&chat, after, limit)
            .await
            .map_err(|e| e.to_string())?;
        let next = ((page.len() as i64) == limit)
            .then(|| page.last().map(message_cursor))
            .flatten();
        // A forward page carries no unread tail: those rows are older than the
        // cursor the caller already holds, and a receipt for them was sent
        // when they were shown.
        let messages = Self::hydrate_message_page(store, client, names, &chat, page, 0).await;
        Ok(Page {
            items: messages,
            next,
        })
    }

    /// Hydrate stored rows into the messages a front end draws.
    ///
    /// One path for both directions: reactions, mentions, quoted authors and
    /// sender names, exactly as the attach load does them, so a page read
    /// forwards and a page read backwards are the same rows with the same
    /// names, and neither leaves an unread tail nobody sends a receipt for.
    async fn hydrate_message_page(
        store: &Arc<ChatStore>,
        client: &Arc<Client>,
        names: &NameBook,
        chat: &Jid,
        page: Vec<oxidezap_chat_store::StoredMessage>,
        unread: i64,
    ) -> Vec<ChatMessage> {
        let mention_lists = crate::mentions::mention_lists_of(&page);
        let quoted_lists = crate::mentions::quoted_mention_lists_of(&page);
        let mut messages: Vec<ChatMessage> = page.into_iter().map(stored_to_chat_message).collect();
        crate::mentions::hydrate_mention_lists(client, names, &mention_lists, &mut messages).await;
        crate::mentions::hydrate_quoted_mention_lists(client, names, &quoted_lists, &mut messages)
            .await;
        Self::hydrate_reactions(store, client, names, chat, &mut messages).await;
        Self::hydrate_quoted_authors(client, names, &mut messages).await;
        if chat.is_group() || chat.is_status_broadcast() {
            Self::hydrate_sender_names(
                store,
                client,
                &mut messages,
                names,
                chat.is_status_broadcast(),
            )
            .await;
        }
        // Exactly what the attach load does to its rows, which is what the
        // paragraph above promises: a page hydrated any other way is one whose
        // unread tail nobody ever sends a receipt for.
        mark_unread_tail(&mut messages, unread.clamp(0, u32::MAX as i64) as u32);
        messages
    }

    /// One page of the chat list, after `after`.
    ///
    /// Rows, not conversations: each carries the newest message the list
    /// previews from and nothing else. What a front end does with the rest of
    /// a chat is ask for it.
    pub fn load_chats(
        &self,
        after: Option<String>,
        limit: i64,
        include_archived: bool,
    ) -> Task<Result<Page<oxidezap_core::Chat>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let Some(live) = session.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let (store, client, names) = (&live.chat_store, &live.client, &live.names);
            let after = after
                .map(|cursor| parse_chat_cursor(&cursor).ok_or("unreadable cursor".to_string()))
                .transpose()?;

            let limit = limit.clamp(1, Self::CHAT_PAGE);
            let entries = store
                .chats_page(include_archived, after, limit)
                .await
                .map_err(|e| e.to_string())?;
            // Off the page as it was read, before the aliases below join it:
            // where the list continues is a position in the store's own order,
            // and a row pulled in from outside the page is not one.
            let next = ((entries.len() as i64) == limit)
                .then(|| entries.last().map(chat_cursor))
                .flatten();
            let entries = Self::with_alias_rows(store, client, names, entries).await;
            // Sized exactly as the attach load sizes it, and for the same
            // reasons: the row previews from its newest message, a read owes
            // a receipt per unread message rather than one for the chat, and
            // the status broadcast is nobody's conversation to open. A page
            // that carried the newest row alone let a window read a chat
            // whose older unread messages then went unacknowledged.
            let mut chats = Self::hydrate_entries(store, client, names, entries, Self::attach_page)
                .await
                .map_err(|e| e.to_string())?;
            Self::hydrate_avatar_sources(client, &mut chats).await;
            Ok(Page { items: chats, next })
        })
    }

    pub(super) async fn hydrate_avatar_sources(client: &Arc<Client>, chats: &mut [Chat]) {
        for chat in chats.iter_mut() {
            chat.avatar_source = None;
            chat.avatar_key = None;
            chat.avatar_loaded = false;
        }
        let groups: Vec<Jid> = chats
            .iter()
            .filter(|chat| chat.is_group)
            .filter_map(|chat| chat.jid.parse().ok())
            .collect();
        if !groups.is_empty()
            && let Ok(pictures) = client
                .groups()
                .get_profile_pictures(groups, whatsapp_rust::features::PictureType::Preview)
                .await
        {
            for picture in pictures {
                if let Some(chat) = chats
                    .iter_mut()
                    .find(|chat| chat.jid == picture.group_jid.to_string())
                {
                    chat.avatar_source = picture.url;
                    chat.avatar_key = picture.photo_id;
                }
            }
            for chat in chats.iter_mut().filter(|chat| chat.is_group) {
                chat.avatar_loaded = true;
            }
        }

        let direct: Vec<(usize, Jid)> = chats
            .iter()
            .enumerate()
            .filter(|(_, chat)| !chat.is_group)
            .filter_map(|(index, chat)| chat.jid.parse().ok().map(|jid| (index, jid)))
            .collect();
        let pictures = whatsapp_rust::futures::future::join_all(direct.into_iter().map(
            |(index, jid)| async move {
                (
                    index,
                    client.contacts().get_profile_picture(&jid, true).await,
                )
            },
        ))
        .await;
        for (index, picture) in pictures {
            if let Ok(picture) = picture {
                chats[index].avatar_loaded = true;
                if let Some(picture) = picture {
                    chats[index].avatar_source = Some(picture.url);
                    chats[index].avatar_key = Some(picture.id);
                }
            }
        }
    }
}

pub(super) fn read_message_range(
    chat_jid: &Jid,
    (ts_secs, ids): ReadBoundary,
) -> wa::sync_action_value::SyncActionMessageRange {
    use whatsapp_rust::features::{message_key, message_range};

    let messages = ids
        .into_iter()
        .filter_map(|(id, from_me, sender)| {
            let participant = if participant_keyed_chat(chat_jid) && !from_me {
                let sender = sender?;
                match sender.parse::<Jid>() {
                    Ok(jid) => Some(jid),
                    Err(e) => {
                        warn!("Invalid chat participant {}: {e}", observe_str(&sender));
                        return None;
                    }
                }
            } else {
                None
            };
            Some((
                message_key(id, chat_jid, from_me, participant.as_ref()),
                ts_secs,
            ))
        })
        .collect();

    message_range(ts_secs, None, messages)
}
