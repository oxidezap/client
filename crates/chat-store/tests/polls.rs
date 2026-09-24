//! The poll-secret fallback: rows compacted before votes existed carry no
//! secret in their proto, so the vote reads the library's own secret index.

// Tests exercise the raw buffa API.
#![allow(clippy::disallowed_methods)]

mod common;

use common::*;
use wacore::store::traits::{MsgSecretEntry, MsgSecretStore};

/// Unsupported V5 rows are still classified as polls; a keyset page lets the
/// session skip them without letting them occupy the requested supported-poll
/// result count.
#[tokio::test]
async fn poll_pages_continue_past_unsupported_variants() {
    let (_store, chat_store) = test_store().await;
    let chat = jid(PEER);
    let supported = wa::Message {
        poll_creation_message_v3: buffa::MessageField::some(Default::default()),
        ..Default::default()
    };
    let unsupported = wa::Message {
        poll_creation_message_v5: buffa::MessageField::some(Default::default()),
        ..Default::default()
    };
    chat_store
        .record_outgoing(&chat, "POLL-LEGACY", &supported, ts(1_700_000_100))
        .unwrap();
    chat_store
        .record_outgoing(&chat, "POLL-V5", &unsupported, ts(1_700_000_200))
        .unwrap();
    chat_store.flush().await.unwrap();

    let first = chat_store
        .poll_messages_page(Some(&chat), None, 1)
        .await
        .unwrap();
    assert_eq!(first[0].id, "POLL-V5");
    assert_eq!(first[0].kind, MessageKind::Poll);
    assert!(first[0].message.as_deref().is_none_or(|message| {
        oxidezap_chat_store::supported_poll_creation_message(message).is_none()
    }));
    let cursor = MessageCursor::from(&first[0]);
    let next = chat_store
        .poll_messages_page(Some(&chat), Some(cursor), 1)
        .await
        .unwrap();
    assert_eq!(next[0].id, "POLL-LEGACY");
    assert_eq!(next[0].kind, MessageKind::Poll);
    assert!(next[0].message.as_deref().is_some_and(|message| {
        oxidezap_chat_store::supported_poll_creation_message(message).is_some()
    }));
}

/// A secret the library captured at receive time is readable back for the
/// vote, even though the stored proto carries no envelope at all.
#[tokio::test]
async fn poll_secret_falls_back_to_the_library_secret_index() {
    let (store, chat_store) = test_store().await;
    let chat = jid(GROUP);
    let sender = jid(PEER);
    store
        .put_msg_secrets(vec![MsgSecretEntry::new(
            &chat,
            &sender,
            "POLL-1",
            [9u8; 32],
            0,
            1_700_000_000,
        )])
        .await
        .expect("seed secret");
    let secret = chat_store
        .poll_secret(&chat, &sender, "POLL-1")
        .await
        .expect("query");
    assert_eq!(secret.as_deref(), Some([9u8; 32].as_slice()));
}

/// Outgoing direct history rows can have an empty sender, while the
/// library indexed the secret under our own JID rather than the peer.
#[tokio::test]
async fn outgoing_direct_poll_with_empty_sender_recovers_secret() {
    let (store, chat_store) = test_store().await;
    let peer = jid(PEER);
    let mine = jid("559900000099@s.whatsapp.net");
    store
        .put_msg_secrets(vec![MsgSecretEntry::new(
            &peer,
            &mine,
            "MY-POLL",
            [5u8; 32],
            0,
            1_700_000_000,
        )])
        .await
        .expect("seed secret");
    assert_eq!(
        chat_store
            .poll_secret(&peer, &jid("559900000077@s.whatsapp.net"), "MY-POLL")
            .await
            .expect("query"),
        Some(vec![5u8; 32])
    );
}

/// An index row captured under PN is still found when the conversation
/// was later canonicalized to LID by the mapping table.
#[tokio::test]
async fn poll_secret_resolves_the_other_chat_alias() {
    let (store, chat_store) = test_store().await;
    let peer = jid(PEER);
    store
        .put_msg_secrets(vec![MsgSecretEntry::new(
            &peer,
            &peer,
            "POLL-ALIAS",
            [4u8; 32],
            0,
            1_700_000_000,
        )])
        .await
        .expect("seed secret");
    add_lid_mapping(&store).await;
    assert_eq!(
        chat_store
            .poll_secret(&jid(PEER_LID), &jid(PEER_LID), "POLL-ALIAS")
            .await
            .expect("query"),
        Some(vec![4u8; 32])
    );
}

/// No row, no secret: the vote is refused rather than guessed.
#[tokio::test]
async fn poll_secret_is_none_without_an_index_row() {
    let (_store, chat_store) = test_store().await;
    let secret = chat_store
        .poll_secret(&jid(GROUP), &jid(PEER), "MISSING")
        .await
        .expect("query");
    assert!(secret.is_none());
}

/// A direct message is filed under the chat itself, mirroring the sender
/// rule the library writes the index with.
#[tokio::test]
async fn poll_secret_in_a_direct_chat_files_under_the_chat() {
    let (store, chat_store) = test_store().await;
    let peer = jid(PEER);
    store
        .put_msg_secrets(vec![MsgSecretEntry::new(
            &peer,
            &peer,
            "POLL-DM",
            [7u8; 32],
            0,
            1_700_000_000,
        )])
        .await
        .expect("seed secret");
    let secret = chat_store
        .poll_secret(&peer, &peer, "POLL-DM")
        .await
        .expect("query");
    assert_eq!(secret.as_deref(), Some([7u8; 32].as_slice()));
}
