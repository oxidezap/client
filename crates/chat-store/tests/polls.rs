//! The poll-secret fallback: rows compacted before votes existed carry no
//! secret in their proto, so the vote reads the library's own secret index.

// Tests exercise the raw buffa API.
#![allow(clippy::disallowed_methods)]

mod common;

use common::*;
use wacore::store::traits::{MsgSecretEntry, MsgSecretStore};

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
