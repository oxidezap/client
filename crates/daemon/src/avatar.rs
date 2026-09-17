//! Profile-picture fetching owned by the daemon.
//!
//! The session resolves the *metadata* — which picture a chat has, and the
//! signed URL that fetches it — and this side turns that into bytes in the
//! media cache and a durable descriptor once they land. The order is the whole
//! contract: a descriptor naming a cache key is written only after the bytes
//! are on disk, so a restart can trust it.

#[cfg(not(target_family = "wasm"))]
mod native;
#[cfg(target_family = "wasm")]
mod web;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::{LazyLock, Mutex};

use portable_atomic::{AtomicU64, Ordering};

use crate::state::StateHub;
use oxidezap_session::AvatarRecorder;

struct AvatarResponse {
    status: u16,
    body: Vec<u8>,
}
use oxidezap_ipc::DaemonMessage;

type InFlightKey = (u64, String, String, u64);

/// In-flight CDN fetches per `(hub_id, jid, picture_id, token)`.
static IN_FLIGHT: LazyLock<Mutex<HashSet<InFlightKey>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

struct InFlightGuard(InFlightKey);
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut in_flight = IN_FLIGHT.lock().unwrap_or_else(|p| p.into_inner());
        in_flight.remove(&self.0);
    }
}

/// One fetch in flight, and what makes it current.
///
/// A second resolution for the same chat supersedes the first, and the older
/// one must not overwrite the newer avatar with a late answer. Account and
/// cache epochs cover the two wipes: an answer for a departed account, or one
/// whose bytes landed after the cache was cleared, is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Selection {
    account: oxidezap_core::AccountId,
    account_generation: usize,
    cache_epoch: usize,
    picture_id: Option<String>,
    source: Option<String>,
    token: u64,
}

/// The current selection per `(hub, jid)`.
///
/// Keyed by [`StateHub::id`] rather than the hub's address: an address is
/// reused the moment a hub drops, so an entry left behind by a departed
/// session was read as the next one's — on a target whose allocator reused
/// addresses eagerly that surfaced as one conversation's picture lookup
/// attributed to another. The id has no second life.
static LATEST: LazyLock<Mutex<HashMap<(u64, String), Selection>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

/// Turn a resolved outcome into cached bytes and a durable descriptor.
///
/// `Found` is the only outcome that changes a picture: a cache hit is
/// published straight away and the descriptor is (re)written, which is how a
/// descriptor lost to an earlier failure heals; a miss fetches, and only
/// writes the descriptor once the bytes are in.
///
/// `NotFound` is the only outcome that removes one, and it is now a fact the
/// library tells apart from a refusal: `NotAuthorized`, `RateOverlimit`,
/// `Unchanged` and a partial response never reach here at all, so a privacy
/// restriction cannot erase a picture that is there.
pub fn resolve(
    hub: &Arc<StateHub>,
    recorder: &AvatarRecorder,
    jid: &str,
    outcome: &oxidezap_core::AvatarOutcome,
) {
    use oxidezap_core::AvatarOutcome;
    let (picture_id, source) = match outcome {
        AvatarOutcome::Found { picture_id, source } => (picture_id.as_str(), source.as_deref()),
        AvatarOutcome::NotFound => {
            remove(hub, recorder, jid);
            return;
        }
    };
    if picture_id.is_empty() {
        // A found picture with no id cannot be addressed or cached; there is
        // nothing to write and nothing to remove.
        return;
    }
    let id = picture_id.to_string();
    let selection = record_selection(hub, jid, Some(&id), source);
    // Account-scoped, like every other durable key in the shared media
    // directory: two local accounts may both know a contact by the same JID
    // and picture id, and an unscoped key would let one account's cached
    // picture be served as the other's, and be billed to neither cleanly.
    let cache_key = crate::media::AccountMedia::new(hub.account_id())
        .key(&oxidezap_core::avatar_cache_key(jid, &id));
    if crate::media::has(&cache_key) {
        oxidezap_session::spawn({
            let hub = Arc::clone(hub);
            let jid = jid.to_string();
            let recorder = recorder.clone();
            let selection = selection.clone();
            async move {
                // The descriptor is rewritten and committed before the
                // readiness is published, so a front end is never told about
                // bytes the durable store does not yet name. A write that did
                // not commit is not announced.
                //
                // `selection.token` is the resolution's order, and the store
                // keeps the highest one: a first picture whose commit lands
                // after a second picture's must not overwrite it.
                if !recorder
                    .record(jid.clone(), id, cache_key.clone(), selection.token)
                    .await
                {
                    if is_current(&hub, &jid, &selection) {
                        recorder.on_failed(&jid);
                        publish_failed(&hub, &jid, true);
                    }
                    return;
                }
                recorder.on_ready(&jid);
                if is_current(&hub, &jid, &selection) {
                    publish_ready(&hub, jid, cache_key);
                }
            }
        });
        return;
    }
    let Some(source) = source else {
        return;
    };
    let in_flight_key = (hub.id(), jid.to_string(), id.clone(), selection.token);
    if !IN_FLIGHT
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(in_flight_key.clone())
    {
        return;
    }
    let source = source.to_string();
    oxidezap_session::spawn({
        let hub = Arc::clone(hub);
        let jid = jid.to_string();
        let recorder = recorder.clone();
        async move {
            let _guard = InFlightGuard(in_flight_key);
            let response = match fetch(&source).await {
                Ok(response) => response,
                Err(_) => {
                    if is_current(&hub, &jid, &selection) {
                        recorder.on_failed(&jid);
                        publish_failed(&hub, &jid, true);
                    }
                    return;
                }
            };
            let retryable = response.status == 429 || (500..600).contains(&response.status);
            let bytes = match accept(response) {
                Ok(bytes) => bytes,
                Err(_) => {
                    if is_current(&hub, &jid, &selection) {
                        if retryable {
                            recorder.on_failed(&jid);
                        }
                        publish_failed(&hub, &jid, retryable);
                    }
                    return;
                }
            };
            // This account's epoch and this account's key: a clear of another
            // account must not refuse this fetch.
            if crate::media::put_since(selection.account, selection.cache_epoch, &cache_key, &bytes)
                .is_err()
            {
                if is_current(&hub, &jid, &selection) {
                    recorder.on_failed(&jid);
                    publish_failed(&hub, &jid, true);
                }
                return;
            }
            if !is_current(&hub, &jid, &selection) {
                return;
            }
            // Cache, then commit, then announce — in that order. The commit
            // is awaited so "durable before visible" is the sequence the code
            // runs rather than a sentence beside it, and its `seq` is the
            // resolution's order so a slower first picture cannot overwrite a
            // faster second one.
            if !recorder
                .record(jid.clone(), id, cache_key.clone(), selection.token)
                .await
            {
                if is_current(&hub, &jid, &selection) {
                    recorder.on_failed(&jid);
                    publish_failed(&hub, &jid, true);
                }
                return;
            }
            recorder.on_ready(&jid);
            if is_current(&hub, &jid, &selection) {
                publish_ready(&hub, jid, cache_key);
            }
        }
    });
}

/// Drop a chat's picture, because WhatsApp says there is none.
///
/// The descriptor goes first, so a restart cannot draw a picture the account
/// no longer has; then every front end is told to draw the placeholder. The
/// cached bytes are left for the budget sweep.
///
/// The removal takes a selection like a fetch does, so a picture resolved
/// *after* this removal is not undone by it: the removal is only applied if
/// nothing newer has been recorded for the chat.
fn remove(hub: &Arc<StateHub>, recorder: &AvatarRecorder, jid: &str) {
    let selection = record_selection(hub, jid, None, None);
    oxidezap_session::spawn({
        let hub = Arc::clone(hub);
        let jid = jid.to_string();
        let recorder = recorder.clone();
        async move {
            if !is_current(&hub, &jid, &selection) {
                return;
            }
            if !recorder.clear(jid.clone(), selection.token).await {
                return;
            }
            if !is_current(&hub, &jid, &selection) {
                return;
            }
            publish_cleared(&hub, &jid);
        }
    });
}

fn publish_ready(hub: &StateHub, jid: String, key: String) {
    let event = oxidezap_core::UiEvent::AvatarReady { jid, key };
    match serde_json::to_string(&DaemonMessage::Session {
        event: Box::new(event),
    }) {
        Ok(frame) => hub.publish_session(frame),
        Err(error) => log::error!("could not serialize avatar readiness: {error}"),
    }
}

/// Tell every front end this chat no longer has a picture.
///
/// An empty key is the placeholder, which is the right drawing for a chat
/// WhatsApp says has no picture.
fn publish_cleared(hub: &StateHub, jid: &str) {
    publish_ready(hub, jid.to_string(), String::new());
}

fn publish_failed(hub: &StateHub, jid: &str, retryable: bool) {
    let event = oxidezap_core::UiEvent::AvatarFailed {
        jid: jid.to_string(),
        retryable,
    };
    match serde_json::to_string(&DaemonMessage::Session {
        event: Box::new(event),
    }) {
        Ok(frame) => hub.publish_session(frame),
        Err(error) => log::error!("could not serialize avatar failure: {error}"),
    }
}

pub fn purge(hub: &StateHub) {
    let mut latest = LATEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    latest.retain(|(id, _), _| *id != hub.id());
    let mut in_flight = IN_FLIGHT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    in_flight.retain(|(id, _, _, _)| *id != hub.id());
}

fn record_selection(
    hub: &StateHub,
    jid: &str,
    picture_id: Option<&str>,
    source: Option<&str>,
) -> Selection {
    let mut latest = LATEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let account = hub.account_id();
    let account_generation = hub.account_generation();
    let cache_epoch = crate::media::epoch(account);
    let key = (hub.id(), jid.to_owned());
    if let Some(selection) = latest.get(&key)
        && selection.account_generation == account_generation
        && selection.cache_epoch == cache_epoch
        && selection.picture_id.as_deref() == picture_id
        && selection.source.as_deref() == source
    {
        return selection.clone();
    }
    let selection = Selection {
        account,
        account_generation,
        cache_epoch,
        picture_id: picture_id.map(str::to_owned),
        source: source.map(str::to_owned),
        token: NEXT_TOKEN.fetch_add(1, Ordering::Relaxed),
    };
    latest.insert(key, selection.clone());
    selection
}

fn is_current(hub: &StateHub, jid: &str, selection: &Selection) -> bool {
    if selection.account_generation != hub.account_generation()
        || selection.cache_epoch != crate::media::epoch(hub.account_id())
    {
        return false;
    }
    let latest = LATEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    latest.get(&(hub.id(), jid.to_owned())) == Some(selection)
}

/// Whether a chat has an avatar fetch recorded as its current one.
///
/// A test-visible answer to "did this path ask about a picture at all",
/// which is the property the history decoupling is about.
#[cfg(test)]
pub(super) fn has_selection(hub: &StateHub, jid: &str) -> bool {
    LATEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains_key(&(hub.id(), jid.to_owned()))
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
    let mut reader = image::ImageReader::new(std::io::Cursor::new(&response.body));
    reader = reader
        .with_guessed_format()
        .map_err(|e| anyhow::anyhow!("invalid avatar format: {e}"))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(oxidezap_core::MAX_AVATAR_DIMENSION);
    limits.max_image_height = Some(oxidezap_core::MAX_AVATAR_DIMENSION);
    limits.max_alloc = Some(oxidezap_core::MAX_AVATAR_PIXELS * 4);
    reader.limits(limits);
    let (width, height) = reader
        .into_dimensions()
        .map_err(|e| anyhow::anyhow!("invalid avatar image: {e}"))?;
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| anyhow::anyhow!("avatar dimensions overflow"))?;
    if pixels > oxidezap_core::MAX_AVATAR_PIXELS {
        anyhow::bail!("avatar has too many pixels");
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(&response.body));
    reader = reader
        .with_guessed_format()
        .map_err(|e| anyhow::anyhow!("invalid avatar format: {e}"))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(oxidezap_core::MAX_AVATAR_DIMENSION);
    limits.max_image_height = Some(oxidezap_core::MAX_AVATAR_DIMENSION);
    limits.max_alloc = Some(oxidezap_core::MAX_AVATAR_PIXELS * 4);
    reader.limits(limits);
    reader
        .decode()
        .map_err(|e| anyhow::anyhow!("invalid avatar image: {e}"))?;
    Ok(response.body)
}

#[cfg(test)]
mod tests {
    use super::{AvatarResponse, accept, is_current, purge, record_selection};
    use crate::state::StateHub;

    #[test]
    fn superseded_avatar_completion_is_rejected() {
        let hub = StateHub::new();
        let first = record_selection(&hub, "jid-superseded", Some("one"), Some("url"));
        let second = record_selection(&hub, "jid-superseded", Some("two"), Some("url"));

        assert!(!is_current(&hub, "jid-superseded", &first));
        assert!(is_current(&hub, "jid-superseded", &second));
    }

    #[test]
    fn identical_avatar_selection_keeps_the_in_flight_completion_current() {
        let hub = StateHub::new();
        let first = record_selection(&hub, "jid-identical", Some("one"), Some("url"));
        let second = record_selection(&hub, "jid-identical", Some("one"), Some("url"));

        assert_eq!(first, second);
        assert!(is_current(&hub, "jid-identical", &first));
    }

    #[test]
    fn purging_a_hub_removes_its_pending_selection() {
        let hub = StateHub::new();
        let selection = record_selection(&hub, "jid-purged", Some("one"), Some("url"));
        purge(&hub);

        assert!(!is_current(&hub, "jid-purged", &selection));
    }

    #[test]
    fn mocked_avatar_fetch_accepts_only_non_empty_success_data() {
        assert_eq!(
            accept(AvatarResponse {
                status: 200,
                body: valid_png(),
            })
            .unwrap(),
            valid_png()
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
        assert!(
            accept(AvatarResponse {
                status: 200,
                body: vec![1, 2, 3]
            })
            .is_err()
        );
        let oversized = image::DynamicImage::new_rgb8(2048, 2048);
        let mut bytes = Vec::new();
        oversized
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        assert!(
            accept(AvatarResponse {
                status: 200,
                body: bytes
            })
            .is_err()
        );
    }

    fn valid_png() -> Vec<u8> {
        let mut bytes = Vec::new();
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }
}
