//! Synthetic Cache Storage and Web Locks tests in an isolated browser runner.

#[allow(dead_code)]
#[path = "../../../gui/src/session/avatar/web.rs"]
mod storage;
#[path = "../../../gui/src/session/avatar/web_lock.rs"]
mod web_lock;

use oxidezap_core::AccountId;
use wasm_bindgen::JsCast as _;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::wasm_bindgen_test;

struct HiddenLocks {
    navigator: js_sys::Object,
    original: wasm_bindgen::JsValue,
}

impl HiddenLocks {
    fn new() -> Self {
        let navigator: js_sys::Object = web_sys::window().unwrap().navigator().unchecked_into();
        let original = js_sys::Object::get_own_property_descriptor(&navigator, &"locks".into());
        let descriptor = js_sys::Object::new();
        js_sys::Reflect::set(
            &descriptor,
            &"value".into(),
            &wasm_bindgen::JsValue::UNDEFINED,
        )
        .unwrap();
        js_sys::Reflect::set(&descriptor, &"configurable".into(), &true.into()).unwrap();
        assert!(
            js_sys::Reflect::define_property(&navigator, &"locks".into(), &descriptor).unwrap()
        );
        Self {
            navigator,
            original,
        }
    }
}

impl Drop for HiddenLocks {
    fn drop(&mut self) {
        if self.original.is_undefined() {
            let _ = js_sys::Reflect::delete_property(&self.navigator, &"locks".into());
        } else {
            let _ = js_sys::Reflect::define_property(
                &self.navigator,
                &"locks".into(),
                self.original.unchecked_ref(),
            );
        }
    }
}

#[wasm_bindgen_test]
async fn browser_avatar_cache_recovers_metadata_and_rejects_pre_clear_writes() {
    let a = AccountId::new(990001).unwrap();
    let b = AccountId::new(990002).unwrap();
    let first = format!("a{a}-a-synthetic-first");
    let second = format!("a{a}-a-synthetic-second");
    storage::set_active_account(Some(a));
    storage::delete_account_storage(Some(a)).await;
    let stamp = storage::persistent_stamp(Some(a)).unwrap();
    storage::write_persistent(&first, b"old", Some(a), stamp)
        .await
        .unwrap();
    assert_eq!(storage::read_persistent(&first).await.unwrap(), b"old");

    // Exercise actual API absence in this isolated test navigator. Reads
    // remain usable; uncoordinated writes and deletions are refused.
    let hidden = HiddenLocks::new();
    assert!(web_lock::lock().await.is_err());
    assert_eq!(storage::read_persistent(&first).await.unwrap(), b"old");
    assert!(
        storage::write_persistent(&second, b"blocked", Some(a), stamp)
            .await
            .is_err()
    );
    assert!(storage::delete_persistent(&first).await.is_err());
    assert_eq!(storage::read_persistent(&first).await.unwrap(), b"old");
    drop(hidden);

    // Interrupted metadata persistence must not turn existing blobs into
    // unaccounted orphans on the next write.
    let local = web_sys::window().unwrap().local_storage().unwrap().unwrap();
    local
        .set_item(
            &format!("oxidezap_avatar_meta_v2_a{}", a.get()),
            "broken metadata",
        )
        .unwrap();
    storage::write_persistent(&second, b"newer", Some(a), stamp)
        .await
        .unwrap();
    assert_eq!(storage::avatar_cache_usage(a).await, (8, 2));
    storage::write_persistent(&second, b"newer", Some(a), stamp)
        .await
        .unwrap();
    assert_eq!(
        storage::avatar_cache_usage(a).await,
        (8, 2),
        "replacement counts once"
    );

    // Simulate a process dying after Cache.put and before metadata commit.
    let orphan = format!("a{a}-a-synthetic-interrupted");
    let caches = web_sys::window().unwrap().caches().unwrap();
    let cache = JsFuture::from(caches.open(&storage::cache_name_for_account(a)))
        .await
        .unwrap()
        .dyn_into::<web_sys::Cache>()
        .unwrap();
    let response = web_sys::Response::new_with_opt_str(Some("orphan")).unwrap();
    JsFuture::from(cache.put_with_str(&storage::cache_url(&orphan), &response))
        .await
        .unwrap();
    local
        .set_item(&format!("oxidezap_avatar_meta_v2_a{}_dirty", a.get()), "1")
        .unwrap();
    storage::write_persistent(&second, b"newer", Some(a), stamp)
        .await
        .unwrap();
    assert_eq!(storage::avatar_cache_usage(a).await, (14, 3));

    // A second local caller waits for the browser's origin-wide lock.
    let held = web_lock::lock().await.unwrap();
    let (done, finished) = futures_channel::oneshot::channel();
    let acquired = std::rc::Rc::new(std::cell::Cell::new(false));
    let acquired_next = std::rc::Rc::clone(&acquired);
    wasm_bindgen_futures::spawn_local(async move {
        let _next = web_lock::lock().await.unwrap();
        acquired_next.set(true);
        let _ = done.send(());
    });
    oxidezap_session::sleep(std::time::Duration::from_millis(10)).await;
    assert!(
        !acquired.get(),
        "the second caller cannot acquire an owned origin lock"
    );
    drop(held);
    finished.await.unwrap();

    // Clear is already queued when an older completion requests the lock.
    let held = web_lock::lock().await.unwrap();
    let (cleared, clear_done) = futures_channel::oneshot::channel();
    wasm_bindgen_futures::spawn_local(async move {
        storage::delete_account_storage(Some(a)).await;
        let _ = cleared.send(());
    });
    oxidezap_session::sleep(std::time::Duration::from_millis(10)).await;
    let late_key = first.clone();
    let (written, write_done) = futures_channel::oneshot::channel();
    wasm_bindgen_futures::spawn_local(async move {
        let result =
            storage::write_persistent(&late_key, b"queued old bytes", Some(a), stamp).await;
        let _ = written.send(result);
    });
    drop(held);
    clear_done.await.unwrap();
    write_done.await.unwrap().unwrap();
    assert!(storage::read_persistent(&first).await.is_none());
    storage::write_persistent(&first, b"late", Some(a), stamp)
        .await
        .unwrap();
    assert!(
        storage::read_persistent(&first).await.is_none(),
        "pre-clear stamp cannot recreate bytes"
    );
    storage::set_active_account(Some(b));
    storage::delete_account_storage(Some(b)).await;
    storage::write_persistent(
        &first,
        b"wrong account",
        Some(a),
        storage::persistent_stamp(Some(a)).unwrap(),
    )
    .await
    .unwrap();
    assert!(storage::read_persistent(&first).await.is_none());
    let caches = web_sys::window().unwrap().caches().unwrap();
    let cache = JsFuture::from(caches.open(&storage::cache_name_for_account(b)))
        .await
        .unwrap()
        .dyn_into::<web_sys::Cache>()
        .unwrap();
    assert_eq!(
        js_sys::Array::from(&JsFuture::from(cache.keys()).await.unwrap()).length(),
        0
    );
    // Recovery of a previously oversized cache has bounded persistent state.
    for start in (0..2050).step_by(50) {
        let mut writes = Vec::new();
        for i in start..(start + 50).min(2050) {
            let key = format!("a{b}-a-recovery-{i}");
            let response = web_sys::Response::new_with_opt_str(Some("x")).unwrap();
            writes.push(JsFuture::from(
                cache.put_with_str(&storage::cache_url(&key), &response),
            ));
        }
        for result in writes {
            result.await.unwrap();
        }
    }
    local
        .set_item(&format!("oxidezap_avatar_meta_v2_a{}_dirty", b.get()), "1")
        .unwrap();
    let arriving = format!("a{b}-a-arriving");
    storage::write_persistent(
        &arriving,
        b"new",
        Some(b),
        storage::persistent_stamp(Some(b)).unwrap(),
    )
    .await
    .unwrap();
    let (bytes, entries) = storage::avatar_cache_usage(b).await;
    assert!(entries <= 2048);
    assert!(bytes <= 64 * 1024 * 1024);
    assert_eq!(
        js_sys::Array::from(&JsFuture::from(cache.keys()).await.unwrap()).length() as u64,
        entries
    );
    assert_eq!(storage::read_persistent(&arriving).await.unwrap(), b"new");
    storage::delete_account_storage(Some(a)).await;
    storage::delete_account_storage(Some(b)).await;
    storage::set_active_account(None);
}
