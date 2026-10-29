use oxidezap_chat_store::ChatStore;
use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
use whatsapp_rust::bot::Bot;
use whatsapp_rust_sqlite_storage::SqliteStore;

use super::durability::ChatStoreDurabilityHook;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
async fn bot_builds_with_chat_store_durability_hook() {
    let backend = SqliteStore::open("file:oxidezap-browser-durability?mode=memory&cache=shared")
        .await
        .expect("in-memory SQLite opens");
    let chat_store = ChatStore::new(&backend).await.expect("chat store opens");
    let result = crate::net::with_platform_plugins(Bot::builder())
        .with_backend(backend)
        .with_inbound_durability_hook(ChatStoreDurabilityHook::new(chat_store.clone()))
        .build()
        .await;

    if let Ok(bot) = &result {
        super::completion::report_shutdown(&bot.client().shutdown().await);
    }
    chat_store.close().await.expect("chat store closes");
    result.expect("bot builds with the SQLite durability probe in a browser");
}

#[wasm_bindgen_test]
async fn terminal_recovery_is_account_scoped_and_does_not_require_a_bot() {
    let backend = SqliteStore::open("file:oxidezap-browser-terminal?mode=memory&cache=shared")
        .await
        .unwrap();
    let database = backend.database();
    let backend = database.create_device().await.unwrap();
    let other = database.create_device().await.unwrap();
    let history = ChatStore::new(&backend).await.unwrap();
    let other_history = ChatStore::new_prepared(&other).await.unwrap();
    assert!(history.logout_message().await.unwrap().is_none());
    history
        .record_logout("Synthetic unlink".into())
        .await
        .unwrap();
    assert_eq!(
        history.logout_message().await.unwrap().as_deref(),
        Some("Synthetic unlink")
    );
    history
        .record_logout("Synthetic unlink".into())
        .await
        .unwrap();
    assert_eq!(
        history.logout_message().await.unwrap().as_deref(),
        Some("Synthetic unlink")
    );
    history.close().await.unwrap();
    assert!(other_history.logout_message().await.unwrap().is_none());
    other_history.close().await.unwrap();
}

#[wasm_bindgen_test]
async fn membership_notice_reloads_with_phone_alias_in_browser() {
    use std::sync::Arc;
    use whatsapp_rust::wacore::stanza::groups::{GroupNotificationAction, GroupParticipantInfo};
    use whatsapp_rust::wacore::types::events::{Event, EventHandler, GroupUpdate};
    let backend = SqliteStore::open("file:oxidezap-browser-membership?mode=memory&cache=shared")
        .await
        .unwrap();
    let store = ChatStore::new(&backend).await.unwrap();
    let bot = crate::net::with_platform_plugins(Bot::builder())
        .with_backend(backend)
        .with_inbound_durability_hook(ChatStoreDurabilityHook::new(store.clone()))
        .build()
        .await
        .unwrap();
    let group = "120363000000000099@g.us";
    let lid = "111000011112222@lid";
    let event = GroupUpdate::builder()
        .group_jid(group.parse().unwrap())
        .notification_id("BROWSER-MEMBERS".to_owned())
        .timestamp(whatsapp_rust::wacore::time::from_secs(1_700_000_000).unwrap())
        .is_lid_addressing_mode(true)
        .action(Box::new(GroupNotificationAction::Add {
            participants: vec![GroupParticipantInfo {
                jid: lid.parse().unwrap(),
                phone_number: Some("12025550123@s.whatsapp.net".parse().unwrap()),
                display_name: None,
                r#type: None,
                lid: None,
                username: None,
                join_time: None,
                group_history_sent_state: None,
            }],
            reason: None,
        }))
        .build();
    store
        .handler()
        .handle_event(Arc::new(Event::GroupUpdate(event)));
    store.flush().await.unwrap();
    let names = crate::names::NameBook::new(None);
    let loaded = super::WhatsAppClient::load_history(&store, &bot.client(), &names)
        .await
        .unwrap();
    let row = &loaded
        .chats
        .iter()
        .find(|c| c.jid == group)
        .unwrap()
        .messages[0];
    assert_eq!(row.content, "Someone added +1 202-555-0123");
    assert!(row.is_read);
    let Some(oxidezap_core::SystemNotice::GroupMembership(event)) = &row.system else {
        panic!("membership")
    };
    assert_eq!(event.participants[0].jid, lid);
    super::completion::report_shutdown(&bot.client().shutdown().await);
    store.close().await.unwrap();
}
