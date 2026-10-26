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
        bot.client().shutdown().await;
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
