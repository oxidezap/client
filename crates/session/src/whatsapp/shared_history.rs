//! Download persisted attachments serially. Failures stay pending across restarts.
use oxidezap_chat_store::{ChatStore, ChatStoreError};
use std::{sync::Arc, time::Duration};
use whatsapp_rust::{
    client::Client,
    download::{ClientDownloadError, DownloadLimits, DownloadPolicyError},
    error::ErrorChainExt,
};

pub(super) fn spawn(
    client: Arc<Client>,
    store: Arc<ChatStore>,
    mut stopping: tokio::sync::watch::Receiver<()>,
) {
    crate::exec::spawn_owned(async move {
        loop {
            // One bounded page and one network request at a time. Cancellation
            // drops only in-flight work; the durable attachment remains pending.
            tokio::select! {
                _ = stopping.changed() => break,
                _ = pass(&client, &store) => {}
            }
            tokio::select! {
                _ = stopping.changed() => break,
                _ = crate::exec::sleep(Duration::from_secs(30)) => {}
            }
        }
    });
}

async fn pass(client: &Arc<Client>, store: &ChatStore) {
    let bundles = match store.pending_history_bundles().await {
        Ok(bundles) => bundles,
        Err(error) => {
            log::warn!("shared history queue read failed: {error}");
            return;
        }
    };
    for pending in bundles {
        if let Err(error) = store.record_history_bundle_attempt(&pending) {
            log::warn!("shared history attempt admission failed: {error}");
            break;
        }
        let Ok(message) = pending.message() else {
            if let Err(error) = store.discard_history_bundle(&pending) {
                log::warn!("shared history discard failed: {error}");
            }
            continue;
        };
        let Some(bundle) = oxidezap_chat_store::normalized_message(&message)
            .message_history_bundle
            .as_option()
        else {
            if let Err(error) = store.discard_history_bundle(&pending) {
                log::warn!("shared history discard failed: {error}");
            }
            continue;
        };
        let result = tokio::select! {
            result = client.download(bundle, DownloadLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024)) => result,
            _ = crate::exec::sleep(Duration::from_secs(60)) => {
                log::warn!("shared history download timed out; attachment remains pending");
                continue;
            }
        };
        match result {
            Ok(bytes) => {
                if let Err(error) = store.apply_history_bundle(&pending, &bytes) {
                    log::warn!("shared history application failed: {error}");
                    if terminal_body_error(&error)
                        && let Err(error) = store.discard_history_bundle(&pending)
                    {
                        log::warn!("shared history discard failed: {error}");
                    }
                }
            }
            Err(error) if terminal_download_error(&error) => {
                if let Err(error) = store.discard_history_bundle(&pending) {
                    log::warn!("shared history discard failed: {error}");
                }
            }
            Err(_) => {
                // No message bodies, media paths or cryptographic keys in logs.
                log::warn!("shared history download failed; attachment remains pending");
            }
        }
    }
    if let Err(error) = store.flush().await {
        log::warn!("shared history commit failed: {error}");
    }
}

fn terminal_download_error(error: &ClientDownloadError) -> bool {
    matches!(
        error,
        ClientDownloadError::ResourcePolicy(DownloadPolicyError::LimitExceeded { .. })
    ) || error
        .http_status()
        .is_some_and(|status| matches!(status, 404 | 410))
}

fn terminal_body_error(error: &ChatStoreError) -> bool {
    matches!(error, ChatStoreError::InvalidGroupHistory(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn writer_pressure_keeps_downloaded_history_retryable() {
        assert!(!terminal_download_error(
            &ClientDownloadError::ResourcePolicy(DownloadPolicyError::StreamingRequired)
        ));
        assert!(terminal_download_error(
            &ClientDownloadError::ResourcePolicy(DownloadPolicyError::LimitExceeded {
                domain: whatsapp_rust::download::MediaByteDomain::Wire,
                limit: 32,
            })
        ));
        assert!(!terminal_body_error(&ChatStoreError::WriteBatchFailed(
            "queue full".into()
        )));
        assert!(terminal_body_error(&ChatStoreError::InvalidGroupHistory(
            "invalid zlib".into()
        )));
    }
}
