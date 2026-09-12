//! Profile-picture fetching owned by the daemon.

#[cfg(not(target_family = "wasm"))]
mod native;
#[cfg(target_family = "wasm")]
mod web;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::{LazyLock, Mutex};

use portable_atomic::{AtomicU64, Ordering};

use crate::state::StateHub;

struct AvatarResponse {
    status: u16,
    body: Vec<u8>,
}
use oxidezap_core::Chat;
use oxidezap_ipc::DaemonMessage;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Selection {
    account_generation: usize,
    cache_epoch: usize,
    token: u64,
}

static LATEST: LazyLock<Mutex<HashMap<(usize, String), Selection>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

pub fn key(jid: &str, id: &str) -> String {
    let jid = safe_component(jid);
    let id = safe_component(id);
    format!("a-{}-{jid}-{}-{id}", jid.len(), id.len())
}

pub fn queue(hub: &Arc<StateHub>, chat: &Chat) {
    let jid = chat.jid.clone();
    let selection = record_selection(hub, &jid);
    let Some(source) = chat.avatar_source.clone() else {
        return;
    };
    let Some(id) = chat.avatar_key.clone() else {
        return;
    };
    let hub = Arc::clone(hub);
    let key = key(&jid, &id);
    if crate::media::has(&key) {
        oxidezap_session::spawn(async move {
            if is_current(&hub, &jid, &selection) {
                hub.signal(&DaemonMessage::AvatarReady { jid, key });
            }
        });
        return;
    }
    oxidezap_session::spawn(async move {
        let Ok(response) = fetch(&source).await else {
            return;
        };
        let Ok(bytes) = accept(response) else { return };
        if crate::media::put_since(selection.cache_epoch, &key, &bytes).is_ok()
            && is_current(&hub, &jid, &selection)
        {
            hub.signal(&DaemonMessage::AvatarReady { jid, key });
        }
    });
}

fn safe_component(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
            result.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(result, ".{byte:02X}");
        }
    }
    result
}

fn record_selection(hub: &StateHub, jid: &str) -> Selection {
    let mut latest = LATEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let selection = Selection {
        account_generation: hub.account_generation(),
        cache_epoch: crate::media::epoch(),
        token: NEXT_TOKEN.fetch_add(1, Ordering::Relaxed),
    };
    latest.insert((hub_id(hub), jid.to_owned()), selection.clone());
    selection
}

fn is_current(hub: &StateHub, jid: &str, selection: &Selection) -> bool {
    if selection.account_generation != hub.account_generation()
        || selection.cache_epoch != crate::media::epoch()
    {
        return false;
    }
    let latest = LATEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    latest.get(&(hub_id(hub), jid.to_owned())) == Some(selection)
}

fn hub_id(hub: &StateHub) -> usize {
    std::ptr::from_ref(hub) as usize
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
    use super::{AvatarResponse, accept, is_current, key, record_selection};
    use crate::state::StateHub;

    #[test]
    fn avatar_keys_are_safe_and_distinct() {
        assert_eq!(key("jid-1", "picture-1"), "a-5-jid-1-9-picture-1");
        assert_ne!(key("jid-1", "picture-1"), key("jid-2", "picture-1"));
        assert_ne!(key("a/b", "picture"), key("a?b", "picture"));
        assert!(!key("../../avatar", "picture").contains('/'));
    }

    #[test]
    fn superseded_avatar_completion_is_rejected() {
        let hub = StateHub::new();
        let first = record_selection(&hub, "jid-superseded");
        let second = record_selection(&hub, "jid-superseded");

        assert!(!is_current(&hub, "jid-superseded", &first));
        assert!(is_current(&hub, "jid-superseded", &second));
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
