//! Native implementation of persistent avatar storage.
//!
//! Shares the filesystem with the daemon directly under the existing
//! media cache directory (`oxidezap_ipc::media_path`). Reads and writes run off the UI
//! thread via `smol::unblock`.

use std::sync::RwLock;

static ACTIVE_ACCOUNT: RwLock<Option<oxidezap_core::AccountId>> = RwLock::new(None);

pub fn set_active_account(account: Option<oxidezap_core::AccountId>) {
    if let Ok(mut lock) = ACTIVE_ACCOUNT.write() {
        *lock = account;
    }
}

pub fn active_account() -> Option<oxidezap_core::AccountId> {
    ACTIVE_ACCOUNT.read().ok().and_then(|lock| *lock)
}

pub fn rotate_account_scope() -> Option<oxidezap_core::AccountId> {
    let old = active_account();
    set_active_account(None);
    old
}

#[allow(dead_code)]
pub fn resolve_account(key: &str) -> Option<oxidezap_core::AccountId> {
    oxidezap_ipc::account_id_of(key).or_else(active_account)
}

pub async fn read_persistent(key: &str) -> Option<Vec<u8>> {
    let path = oxidezap_ipc::media_path(key)?;
    smol::unblock(move || std::fs::read(path).ok()).await
}

pub async fn delete_persistent(key: &str) -> Result<(), String> {
    if let Some(path) = oxidezap_ipc::media_path(key) {
        let _ = smol::unblock(move || std::fs::remove_file(path)).await;
    }
    Ok(())
}

#[allow(dead_code)]
pub async fn write_persistent(
    _key: &str,
    _bytes: &[u8],
    _account: Option<oxidezap_core::AccountId>,
) -> Result<(), String> {
    // Daemon already persists media atomically; no redundant write needed on native.
    Ok(())
}

pub async fn delete_account_storage(_account: Option<oxidezap_core::AccountId>) {
    // Daemon manages file deletion on account wipe
}

pub async fn clear_cache_storage(_account: Option<oxidezap_core::AccountId>) {
    // Daemon manages file deletion on media cache clear
}

#[allow(dead_code)]
pub async fn avatar_cache_usage(_account: oxidezap_core::AccountId) -> (u64, u64) {
    (0, 0)
}

pub async fn purge_legacy_caches() {
    // Native does not use browser Cache Storage
}

pub fn spawn_task(fut: impl std::future::Future<Output = ()> + Send + 'static) {
    smol::spawn(fut).detach();
}
