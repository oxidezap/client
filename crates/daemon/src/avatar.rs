//! Profile-picture fetching owned by the daemon.

#[cfg(not(target_family = "wasm"))]
mod native;
#[cfg(target_family = "wasm")]
mod web;

use std::sync::Arc;

use crate::state::StateHub;

struct AvatarResponse {
    status: u16,
    body: Vec<u8>,
}
use oxidezap_core::Chat;
use oxidezap_ipc::DaemonMessage;

pub fn key(id: &str) -> String {
    let mut result = String::from("a-");
    for byte in id.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
            result.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(result, ".{byte:02X}");
        }
    }
    result
}

pub fn queue(hub: &Arc<StateHub>, chat: &Chat) {
    let Some(source) = chat.avatar_source.clone() else {
        return;
    };
    let Some(id) = chat.avatar_key.clone() else {
        return;
    };
    let jid = chat.jid.clone();
    let hub = Arc::clone(hub);
    let key = key(&id);
    if crate::media::has(&key) {
        oxidezap_session::spawn(async move {
            hub.signal(&DaemonMessage::AvatarReady { jid, key });
        });
        return;
    }
    oxidezap_session::spawn(async move {
        let Ok(response) = fetch(&source).await else {
            return;
        };
        let Ok(bytes) = accept(response) else { return };
        if crate::media::put(&key, &bytes).is_ok() {
            hub.signal(&DaemonMessage::AvatarReady { jid, key });
        }
    });
}

async fn fetch(url: &str) -> anyhow::Result<AvatarResponse> {
    #[cfg(not(target_family = "wasm"))]
    {
        let url = url.to_string();
        let (status, body) = oxidezap_session::unblock(move || native::fetch(&url)).await??;
        Ok(AvatarResponse { status, body })
    }
    #[cfg(target_family = "wasm")]
    {
        let (status, body) = web::fetch(url).await?;
        Ok(AvatarResponse { status, body })
    }
}

fn accept(response: AvatarResponse) -> anyhow::Result<Vec<u8>> {
    if !(200..300).contains(&response.status) {
        anyhow::bail!("avatar fetch returned {}", response.status);
    }
    if response.body.is_empty() {
        anyhow::bail!("avatar response was empty");
    }
    Ok(response.body)
}

#[cfg(test)]
mod tests {
    use super::{AvatarResponse, accept, key};

    #[test]
    fn avatar_keys_are_safe_and_distinct() {
        assert_eq!(key("picture-1"), "a-picture-1");
        assert_ne!(key("a/b"), key("a?b"));
        assert!(!key("../../avatar").contains('/'));
    }

    #[test]
    fn mocked_avatar_fetch_accepts_only_non_empty_success_data() {
        assert_eq!(
            accept(AvatarResponse {
                status: 200,
                body: vec![1, 2, 3]
            })
            .unwrap(),
            vec![1, 2, 3]
        );
        assert!(
            accept(AvatarResponse {
                status: 404,
                body: vec![1]
            })
            .is_err()
        );
        assert!(
            accept(AvatarResponse {
                status: 200,
                body: Vec::new()
            })
            .is_err()
        );
    }
}
