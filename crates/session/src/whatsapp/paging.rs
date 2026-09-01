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

use oxidezap_core::ChatMessage;

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
        let chat_store = self.chat_store.clone();
        let client_handle = self.client_handle.clone();
        let names = self.names.clone();
        self.exec.spawn(async move {
            let Some(store) = chat_store.lock().await.clone() else {
                return Err("no chat store yet".to_string());
            };
            let Some(client) = client_handle.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let Some(names) = names.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            Self::message_page(&store, &client, &names, jid, before, limit).await
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
        let mut messages: Vec<ChatMessage> = page.into_iter().map(stored_to_chat_message).collect();
        Self::hydrate_reactions(store, client, names, &chat, &mut messages).await;
        Self::canonicalize_quoted_authors(client, names, &mut messages).await;
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
        Ok(Page {
            items: messages,
            next,
        })
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
    ) -> Task<Result<Page<oxidezap_core::Chat>, String>> {
        let chat_store = self.chat_store.clone();
        let client_handle = self.client_handle.clone();
        let names = self.names.clone();
        self.exec.spawn(async move {
            let Some(store) = chat_store.lock().await.clone() else {
                return Err("no chat store yet".to_string());
            };
            let Some(client) = client_handle.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let Some(names) = names.lock().await.clone() else {
                return Err("no session yet".to_string());
            };
            let after = after
                .map(|cursor| parse_chat_cursor(&cursor).ok_or("unreadable cursor".to_string()))
                .transpose()?;

            let limit = limit.clamp(1, Self::CHAT_PAGE);
            let entries = store
                .chats_page(false, after, limit)
                .await
                .map_err(|e| e.to_string())?;
            // Off the page as it was read, before the aliases below join it:
            // where the list continues is a position in the store's own order,
            // and a row pulled in from outside the page is not one.
            let next = ((entries.len() as i64) == limit)
                .then(|| entries.last().map(chat_cursor))
                .flatten();
            let entries = Self::with_alias_rows(&store, &client, &names, entries).await;
            // Sized exactly as the attach load sizes it, and for the same
            // reasons: the row previews from its newest message, a read owes
            // a receipt per unread message rather than one for the chat, and
            // the status broadcast is nobody's conversation to open. A page
            // that carried the newest row alone let a window read a chat
            // whose older unread messages then went unacknowledged.
            let chats = Self::hydrate_entries(&store, &client, &names, entries, Self::attach_page)
                .await
                .map_err(|e| e.to_string())?;
            Ok(Page { items: chats, next })
        })
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
