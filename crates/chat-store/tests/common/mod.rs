//! The store every integration test opens, the events it feeds in, and the
//! handful of envelopes those events are built from.
//!
//! Each test file beside this one is its own binary and compiles this module
//! into itself, so it uses whatever subset of these it needs — hence the
//! blanket allow below, which is about the compilation unit rather than about
//! anything here being unused.
//!
//! The builders are plain functions with the defaults production sends: a
//! receipt is live rather than drained from the offline queue, an ack names a
//! message stanza, a revoke names only the id it takes back. A test that
//! needs one of those to differ says so at its own call site, which is the
//! point — the eleven-line `Receipt::builder()` chain said all six of them
//! again every time and hid the one that varied.

#![allow(dead_code, unused_imports)]

pub use std::sync::Arc;

pub use buffa::MessageField;
pub use chrono::{DateTime, Datelike, TimeZone, Utc};
pub use diesel::RunQueryDsl;
pub use oxidezap_chat_store::{
    ChatCursor, ChatStore, MessageCursor, MessageKind, MessageStatus, StoreChange, db_err,
};
pub use std::time::Duration;
pub use wacore::proto_helpers::MessageBuilderExt;
pub use wacore::types::events::{
    BatchOrigin, Event, InboundMessage, LazyHistorySync, MessageBatch, Receipt, ServerAck,
};
pub use wacore::types::message::{MessageInfo, MessageSource};
pub use wacore::types::presence::ReceiptType;
pub use wacore_binary::Jid;
pub use waproto::whatsapp as wa;
pub use whatsapp_rust_sqlite_storage::SqliteStore;

/// The peer every one-to-one test talks to, and the group.
pub const PEER: &str = "559900000001@s.whatsapp.net";
pub const GROUP: &str = "120363000000000001@g.us";
/// The same peer under their LID identity.
pub const PEER_LID: &str = "111000011112222@lid";

/// A fresh store, private to the calling test.
///
/// Each one gets its own shared-cache in-memory database, named after the
/// process and a counter: the tests run concurrently in one process, and a
/// single `:memory:` would either be shared by all of them or dropped the
/// moment the first connection closed.
pub async fn test_store() -> (SqliteStore, Arc<ChatStore>) {
    use portable_atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let db_name = format!(
        "file:memdb_chat_store_{}_{}?mode=memory&cache=shared",
        std::process::id(),
        id
    );
    let store = SqliteStore::new(&db_name).await.expect("create store");
    let chat_store = ChatStore::new(&store).await.expect("create chat store");
    (store, chat_store)
}

pub fn jid(s: &str) -> Jid {
    s.parse().expect("valid test JID")
}

/// A whole wire second as an instant.
///
/// The tests date everything from 1_700_000_000 and step by seconds, so this
/// is the one spelling of `Utc.timestamp_opt(secs, 0).unwrap()` — which was
/// written out at every one of a hundred and thirty-seven sites.
pub fn ts(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(secs, 0).expect("a representable test instant")
}

pub fn incoming_info(chat: &str, sender: &str, id: &str, ts_secs: i64) -> MessageInfo {
    MessageInfo {
        source: MessageSource {
            chat: jid(chat),
            sender: jid(sender),
            is_from_me: false,
            is_group: chat.ends_with("@g.us"),
            ..Default::default()
        },
        id: id.to_string(),
        timestamp: ts(ts_secs),
        ..Default::default()
    }
}

pub fn message_event(msg: wa::Message, info: MessageInfo) -> Event {
    Event::Messages(
        MessageBatch::builder()
            .messages(Arc::from([InboundMessage::builder()
                .message(Arc::new(msg))
                .info(Arc::new(info))
                .build()]))
            .origin(BatchOrigin::Live)
            .build(),
    )
}

/// Hand `events` to the store's handler and wait for the batch to land.
///
/// The writer queue is ordered and asynchronous, so a test that read straight
/// after handing an event over would be racing it; `flush` is the boundary.
pub async fn feed(chat_store: &ChatStore, events: impl IntoIterator<Item = Event>) {
    let handler = chat_store.handler();
    for event in events {
        handler.handle_event(Arc::new(event));
    }
    chat_store.flush().await.expect("flush");
}

pub fn history_sync_event(history: wa::HistorySync) -> Event {
    use buffa::Message as _;
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write;

    let raw = history.encode_to_vec();
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&raw).unwrap();
    Event::HistorySync(Box::new(LazyHistorySync::new(
        enc.finish().unwrap().into(),
        raw.len(),
        wa::history_sync::HistorySyncType::RECENT as i32,
        None,
        None,
    )))
}

/// A receipt for `ids`, as it arrives live.
///
/// `is_group` is left defaulted the way production receipts leave it — the
/// store derives groupness from the chat JID, and a fixture that filled the
/// flag in would stop testing that.
pub fn receipt(chat: Jid, sender: Jid, ids: &[&str], ty: ReceiptType, at: DateTime<Utc>) -> Event {
    built_receipt(chat, sender, ids, ty, at, false)
}

/// The same receipt, drained from the server's offline queue on reconnect
/// rather than delivered live — which is a different instruction: it may be
/// arbitrarily stale, and the store treats it accordingly.
pub fn offline_receipt(
    chat: Jid,
    sender: Jid,
    ids: &[&str],
    ty: ReceiptType,
    at: DateTime<Utc>,
) -> Event {
    built_receipt(chat, sender, ids, ty, at, true)
}

fn built_receipt(
    chat: Jid,
    sender: Jid,
    ids: &[&str],
    ty: ReceiptType,
    at: DateTime<Utc>,
    offline: bool,
) -> Event {
    Event::Receipt(
        Receipt::builder()
            .source(MessageSource {
                chat,
                sender,
                ..Default::default()
            })
            .message_ids(ids.iter().map(|id| (*id).to_string()).collect())
            .timestamp(at)
            .r#type(ty)
            .offline(offline)
            .build(),
    )
}

/// A read receipt from the chat itself — the reader's own other device, in a
/// one-to-one thread.
pub fn read_receipt(chat: &str, ids: &[&str], ts_secs: i64) -> Event {
    receipt(jid(chat), jid(chat), ids, ReceiptType::Read, ts(ts_secs))
}

/// A receipt from a peer identity, which is both the chat and the sender.
pub fn peer_receipt(source: Jid, ids: &[&str], ty: ReceiptType, ts_secs: i64) -> Event {
    receipt(source.clone(), source, ids, ty, ts(ts_secs))
}

/// A peer's linked device as the binary decoder yields it: `user:48@lid`,
/// carrying the LID domain-type byte in `agent`.
pub fn companion(user: &str, device: u16) -> Jid {
    Jid {
        user: user.into(),
        server: wacore_binary::Server::Lid,
        agent: 1,
        device,
        integrator: 0,
    }
}

/// The server acknowledging the outgoing message `id` in `chat`.
///
/// `class` is always `"message"` here: acks cover every outgoing stanza
/// class, and the store filters on it before correlating ids, so an ack for
/// anything else would simply be ignored.
pub fn ack(id: &str, chat: Jid) -> Event {
    built_ack(id, Some(chat), None, None)
}

/// The same ack, carrying the server timestamp — which is the authoritative
/// send instant, not the local one the row was recorded with.
pub fn ack_at(id: &str, chat: Jid, at: DateTime<Utc>) -> Event {
    built_ack(id, Some(chat), Some(at), None)
}

/// The server rejecting the outgoing message `id` with a nack code.
pub fn nack(id: &str, chat: Jid, code: &str) -> Event {
    built_ack(id, Some(chat), None, Some(code))
}

fn built_ack(id: &str, chat: Option<Jid>, at: Option<DateTime<Utc>>, code: Option<&str>) -> Event {
    Event::ServerAck(
        ServerAck::builder()
            .id(id.to_string())
            .class("message".to_string())
            .maybe_from(chat)
            .maybe_timestamp(at)
            .maybe_error(code.map(str::to_string))
            .build(),
    )
}

/// The protocol message that takes `id` back.
pub fn revoke(id: &str) -> wa::Message {
    revoke_key(wa::MessageKey {
        id: Some(id.into()),
        ..Default::default()
    })
}

/// A revoke naming more of its target's key than the id — who wrote it, or
/// that the reader did. The store reads those to decide whose tombstone this
/// becomes, so a test about that says the whole key.
pub fn revoke_key(key: wa::MessageKey) -> wa::Message {
    wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage {
            key: MessageField::some(key),
            r#type: Some(wa::message::protocol_message::Type::REVOKE),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A delete-for-me from app-state sync: one message dropped on this account's
/// devices only, leaving everyone else's copy alone.
pub fn delete_for_me(chat: Jid, id: &str, from_me: bool, at: DateTime<Utc>) -> Event {
    Event::DeleteMessageForMeUpdate(
        wacore::types::events::DeleteMessageForMeUpdate::builder()
            .chat_jid(chat)
            .message_id(id.to_string())
            .from_me(from_me)
            .timestamp(at)
            .action(Box::new(
                wa::sync_action_value::DeleteMessageForMeAction::default(),
            ))
            .from_full_sync(false)
            .build(),
    )
}

/// The range an app-state action covers: everything up to and including
/// `ts_secs`, with no message keys listed.
pub fn range_up_to(ts_secs: i64) -> MessageField<wa::sync_action_value::SyncActionMessageRange> {
    MessageField::some(wa::sync_action_value::SyncActionMessageRange {
        last_message_timestamp: Some(ts_secs),
        ..Default::default()
    })
}

/// The whole-chat mark-read (or mark-unread) another device performed.
pub fn mark_read_event(chat: &str, read: bool, ts_secs: i64) -> Event {
    Event::MarkChatAsReadUpdate(
        wacore::types::events::MarkChatAsReadUpdate::builder()
            .jid(jid(chat))
            .timestamp(ts(ts_secs))
            .action(Box::new(wa::sync_action_value::MarkChatAsReadAction {
                read: Some(read),
                ..Default::default()
            }))
            .from_full_sync(false)
            .build(),
    )
}

/// Learn PEER <-> PEER_LID in the device store's mapping table, the alias
/// index the chat-store resolves against.
pub async fn add_lid_mapping(store: &SqliteStore) {
    use wacore::store::traits::{LidPnMappingEntry, ProtocolStore};
    // The mapping table's FK needs the device row the client normally creates.
    store.create_new_device().await.expect("create device");
    store
        .put_lid_mapping(&LidPnMappingEntry {
            lid: "111000011112222".into(),
            phone_number: "559900000001".into(),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
            learning_source: "usync".into(),
        })
        .await
        .expect("put mapping");
}
