mod common;
use common::*;
use wa::message_association::AssociationType as Type;

fn image(caption: &str) -> wa::Message {
    let mut media = wa::message::ImageMessage::default();
    media.caption = Some(caption.into());
    media.direct_path = Some("/synthetic/image".into());
    let mut msg = wa::Message::default();
    msg.image_message = MessageField::some(media);
    msg
}

fn child(kind: Type, parent: &str, media: wa::Message) -> wa::Message {
    let mut key = wa::MessageKey::default();
    key.id = Some(parent.into());
    key.from_me = Some(true);
    let mut assoc = wa::MessageAssociation::default();
    assoc.association_type = Some(kind);
    assoc.parent_message_key = MessageField::some(key);
    let mut context = wa::MessageContextInfo::default();
    context.message_association = MessageField::some(assoc);
    let mut wrapper = wa::message::FutureProofMessage::default();
    wrapper.message = MessageField::some(media);
    let mut msg = wa::Message::default();
    msg.associated_child_message = MessageField::some(wrapper);
    msg.message_context_info = MessageField::some(context);
    msg
}

fn incoming(id: &str, at: i64, msg: wa::Message) -> Event {
    message_event(msg, incoming_info(PEER, PEER, id, at))
}

#[tokio::test]
async fn parent_and_redelivered_hd_image_have_one_timeline_entry_and_badge() {
    let (_db, store) = test_store().await;
    let peer = jid(PEER);
    let hd = child(
        Type::HD_IMAGE_DUAL_UPLOAD,
        "parent",
        image("match rendition"),
    );
    feed(
        &store,
        [
            incoming("parent", 100, image("match parent")),
            incoming("hd", 200, hd.clone()),
            incoming("hd", 200, hd),
        ],
    )
    .await;
    let (page, unread) = store.page_with_unread(&peer, None, 1).await.unwrap();
    assert_eq!(page[0].id, "parent");
    assert_eq!(unread, 1);
    let chat = store.chats(false, 1).await.unwrap().remove(0);
    assert_eq!(chat.last_message_preview.as_deref(), Some("match parent"));
    assert_eq!(chat.last_message_at, Some(ts(100)));
    assert_eq!(store.messages_by_arrival(None, 5).await.unwrap().len(), 1);
    #[cfg(feature = "search")]
    assert_eq!(store.search_messages("match", 10).await.unwrap().len(), 1);
    let Event::MarkChatAsReadUpdate(mut read) = mark_read_event(PEER, true, 99) else {
        unreachable!()
    };
    read.action.message_range = range_up_to(99);
    feed(&store, [Event::MarkChatAsReadUpdate(read)]).await;
    assert_eq!(store.chats(false, 1).await.unwrap()[0].unread_count, 1);
}

#[tokio::test]
async fn orphan_keeps_its_parent_and_media_across_reload_and_history() {
    let (db, store) = test_store().await;
    let peer = jid(PEER);
    let hd = child(Type::HD_IMAGE_DUAL_UPLOAD, "parent", image("orphan"));
    feed(&store, [incoming("hd", 200, hd.clone())]).await;
    assert!(store.messages(&peer, None, 3).await.unwrap().is_empty());
    let copies = store.pending_media_messages(Some(&peer), 10).await.unwrap();
    assert_eq!(copies[0].id, "hd");
    assert_eq!(copies[0].message.as_deref(), Some(&hd));
    let mut conv = wa::Conversation::default();
    conv.id = PEER.into();
    for (id, time, msg) in [("hd", 200, hd), ("parent", 100, image("parent"))] {
        let mut key = wa::MessageKey::default();
        key.id = Some(id.into());
        let mut wmi = wa::WebMessageInfo::default();
        wmi.key = MessageField::some(key);
        wmi.message_timestamp = Some(time);
        wmi.message = MessageField::some(msg);
        let mut entry = wa::HistorySyncMsg::default();
        entry.message = MessageField::some(wmi);
        conv.messages.push(entry);
    }
    let mut history = wa::HistorySync::default();
    history.conversations.push(conv);
    feed(&store, [history_sync_event(history)]).await;
    let reopened = ChatStore::new(&db).await.unwrap();
    assert_eq!(
        reopened.messages(&peer, None, 3).await.unwrap()[0].id,
        "parent"
    );
    let copies = reopened
        .media_renditions(&peer, "parent", &peer, false)
        .await
        .unwrap();
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0].id, "hd");
    assert!(
        reopened
            .media_renditions(&peer, "parent", &peer, true)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        reopened
            .media_renditions(&peer, "parent", &jid("100003@s.whatsapp.net"), false)
            .await
            .unwrap()
            .is_empty()
    );
}

#[test]
fn dual_upload_types_require_the_matching_media_and_a_parent_key() {
    use oxidezap_chat_store::{is_control_only, rendition_parent_key};
    let mut video = wa::Message::default();
    video.video_message = MessageField::some(Default::default());
    // The live session consults this predicate before building a bubble.
    for kind in [
        Type::HD_VIDEO_DUAL_UPLOAD,
        Type::HEVC_VIDEO_DUAL_UPLOAD,
        Type::AV1_VIDEO_DUAL_UPLOAD,
        Type::STREAMED_HD_VIDEO_DUAL_UPLOAD,
    ] {
        let msg = child(kind, "video", video.clone());
        assert!(is_control_only(&msg));
        assert_eq!(
            rendition_parent_key(&msg).unwrap().id.as_deref(),
            Some("video")
        );
        assert!(!is_control_only(&child(kind, "image", image("regular"))));
    }
    for kind in [Type::MEDIA_ALBUM, Type::MOTION_PHOTO] {
        assert!(!is_control_only(&child(kind, "parent", image("distinct"))));
    }
    assert!(!is_control_only(&child(
        Type::HD_IMAGE_DUAL_UPLOAD,
        "",
        image("unlinked")
    )));
}

#[tokio::test]
async fn startup_reclassifies_old_rows_once_and_preserves_the_media() {
    let (db, store) = test_store().await;
    let hd = child(Type::HD_IMAGE_DUAL_UPLOAD, "parent", image("legacy"));
    feed(
        &store,
        [
            incoming("parent", 100, image("standard")),
            incoming("hd", 200, hd.clone()),
        ],
    )
    .await;
    db.shared()
        .run(|conn| {
            use diesel::connection::SimpleConnection;
            conn.batch_execute(
                "UPDATE messages SET rendition_parent_id=NULL;
             UPDATE chats SET unread_count=2,last_message_ts=200000,last_message_preview='legacy';
             DELETE FROM chat_store_meta WHERE key='media_renditions'",
            )
            .map_err(db_err)
        })
        .await
        .unwrap();
    for _ in 0..2 {
        let reopened = ChatStore::new(&db).await.unwrap();
        let chat = reopened.chats(false, 1).await.unwrap().remove(0);
        assert_eq!(chat.unread_count, 1);
        assert_eq!(chat.last_message_at, Some(ts(100)));
        assert_eq!(chat.last_message_preview.as_deref(), Some("standard"));
        let media = reopened.message(&jid(PEER), "hd").await.unwrap().unwrap();
        assert_eq!(media.message.as_deref(), Some(&hd));
        #[cfg(feature = "search")]
        assert!(
            reopened
                .search_messages("legacy", 1)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
