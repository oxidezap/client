//! Native implementation of persistent avatar storage.
//!
//! Shares the filesystem with the daemon directly under the existing
//! media cache directory (`oxidezap_ipc::media_path`). Reads run off the UI
//! thread via `smol::unblock`.

pub async fn read_persistent(key: &str) -> Option<Vec<u8>> {
    let path = oxidezap_ipc::media_path(key)?;
    smol::unblock(move || std::fs::read(path).ok()).await
}

#[allow(dead_code)]
pub async fn write_persistent(key: &str, bytes: &[u8]) -> Result<(), String> {
    let path =
        oxidezap_ipc::media_path(key).ok_or_else(|| "no media cache path available".to_string())?;
    let bytes = bytes.to_vec();
    smol::unblock(move || {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&path, bytes).map_err(|e| e.to_string())
    })
    .await
}

pub async fn delete_account_storage() {
    // Daemon manages file deletion on account wipe
}

pub async fn clear_cache_storage() {
    // Daemon manages file deletion on media cache clear
}

pub fn spawn_task(fut: impl std::future::Future<Output = ()> + Send + 'static) {
    smol::spawn(fut).detach();
}
