//! Durable avatar descriptors: the pointer from a chat to the bytes it shows.
//!
//! The media cache already addresses a picture deterministically, so the bytes
//! survive a restart until the budget sweep reclaims them. What used to be lost
//! was the pointer — nothing durable said which picture id belonged to a JID,
//! so the process drew initials over bytes it still had. These tests are about
//! that pointer, and about it never naming bytes that are not there.

// Tests exercise the raw buffa API.
#![allow(clippy::disallowed_methods)]

mod common;

use common::*;

/// The failure the table exists for: reopening the file must still say which
/// picture a chat shows, without any network.
#[tokio::test]
async fn an_avatar_descriptor_survives_reopening_the_store() {
    let (store, chat_store) = test_store().await;

    chat_store
        .record_avatar(&jid(PEER), "picture-1", "a-kept")
        .unwrap();
    chat_store.flush().await.unwrap();

    let reopened = ChatStore::new(&store).await.unwrap();
    let descriptors = reopened.avatar_descriptors().await.unwrap();
    let descriptor = descriptors
        .iter()
        .find(|descriptor| descriptor.jid == jid(PEER))
        .expect("the descriptor outlived the process");
    assert_eq!(descriptor.picture_id, "picture-1");
    assert_eq!(descriptor.cache_key, "a-kept");
}

/// A picture change overwrites in place: one row per chat, naming the newest
/// picture, never a growing history of them.
#[tokio::test]
async fn the_newest_picture_replaces_the_previous_descriptor() {
    let (_store, chat_store) = test_store().await;

    chat_store
        .record_avatar(&jid(PEER), "picture-1", "a-one")
        .unwrap();
    chat_store
        .record_avatar(&jid(PEER), "picture-2", "a-two")
        .unwrap();
    chat_store.flush().await.unwrap();

    let descriptors = chat_store.avatar_descriptors().await.unwrap();
    assert_eq!(descriptors.len(), 1, "one chat, one current picture");
    assert_eq!(descriptors[0].picture_id, "picture-2");
    assert_eq!(descriptors[0].cache_key, "a-two");
}

/// A picture removed server-side must not leave a durable pointer behind: the
/// next start would draw a picture WhatsApp says is gone.
#[tokio::test]
async fn a_removed_picture_drops_its_descriptor() {
    let (_store, chat_store) = test_store().await;
    chat_store
        .record_avatar(&jid(PEER), "picture-1", "a-one")
        .unwrap();
    chat_store.flush().await.unwrap();

    chat_store.clear_avatar(&jid(PEER)).unwrap();
    chat_store.flush().await.unwrap();

    assert!(
        chat_store.avatar_descriptors().await.unwrap().is_empty(),
        "the pointer outlived the picture it named"
    );
}

/// Nothing in the row is a signed URL or a token. The source is the thing that
/// expires, and a credential in the store is a credential in every backup.
#[tokio::test]
async fn no_source_url_is_stored() {
    let (_store, chat_store) = test_store().await;
    chat_store
        .record_avatar(&jid(PEER), "picture-1", "a-one")
        .unwrap();
    chat_store.flush().await.unwrap();

    let descriptors = chat_store.avatar_descriptors().await.unwrap();
    let rendered = format!("{descriptors:?}");
    assert!(
        !rendered.contains("http"),
        "a descriptor carried something URL-shaped: {rendered}"
    );
    assert!(!rendered.to_lowercase().contains("token"));
}
