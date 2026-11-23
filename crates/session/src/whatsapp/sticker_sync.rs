//! Account favorites delivered by the linked-device app-state collection.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use oxidezap_core::{DownloadableMedia, PickerSticker};
use whatsapp_rust::{
    AppStateResyncMode, WAPatchName,
    wacore::{
        download::MediaType,
        types::events::{Event, EventHandler, EventInterest, EventKind},
    },
    waproto::{buffa::MessageField, whatsapp as wa},
};

use whatsapp_rust::wacore_binary::JidExt as _;

use super::{Task, WhatsAppClient};

struct Entry {
    timestamp: i64,
    lottie: bool,
    animated: Option<bool>,
    media: Option<DownloadableMedia>,
}

#[derive(Default)]
pub(super) struct StickerSync {
    entries: Mutex<BTreeMap<String, Entry>>,
    loaded: tokio::sync::Mutex<bool>,
}

impl EventHandler for StickerSync {
    fn interest(&self) -> EventInterest {
        EventInterest::of(&[EventKind::FavoriteStickerUpdate])
    }

    fn handle_event(&self, event: Arc<Event>) {
        let Event::FavoriteStickerUpdate(update) = &*event else {
            return;
        };
        let timestamp = update.timestamp.as_millis();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if entries
            .get(&update.filehash)
            .is_some_and(|entry| entry.timestamp > timestamp)
        {
            return;
        }
        let action = &update.action;
        let media = if action.is_favorite == Some(true) {
            (|| {
                let direct_path = action.direct_path.clone().filter(|path| !path.is_empty())?;
                let media_key = action.media_key.clone().filter(|key| key.len() == 32)?;
                let file_enc_sha256 = action
                    .file_enc_sha256
                    .clone()
                    .filter(|hash| hash.len() == 32)?;
                let file_sha256 = STANDARD
                    .decode(&update.filehash)
                    .ok()
                    .filter(|hash| hash.len() == 32)?;
                Some(DownloadableMedia {
                    direct_path,
                    media_key,
                    file_enc_sha256,
                    file_sha256,
                    static_url: None,
                    is_thumbnail: false,
                    file_length: action.file_length.unwrap_or_default(),
                    mime_type: action
                        .mimetype
                        .clone()
                        .unwrap_or_else(|| "image/webp".into()),
                    duration_secs: None,
                    download_type: MediaType::Sticker,
                })
            })()
        } else {
            None
        };
        let media = if action.is_favorite == Some(true) && media.is_none() {
            entries
                .get(&update.filehash)
                .and_then(|entry| entry.media.clone())
        } else {
            media
        };
        let lottie = action.is_lottie.unwrap_or_else(|| {
            entries
                .get(&update.filehash)
                .is_some_and(|entry| entry.lottie)
        });
        let animated = entries
            .get(&update.filehash)
            .filter(|entry| media.is_some() && entry.media == media && entry.lottie == lottie)
            .and_then(|entry| entry.animated);
        entries.insert(
            update.filehash.clone(),
            Entry {
                timestamp,
                media,
                lottie,
                animated,
            },
        );
    }
}

impl StickerSync {
    pub(super) fn remember_animation(&self, id: &str, encrypted_hash: &[u8], animated: bool) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(entry) = entries.get_mut(id)
            && entry
                .media
                .as_ref()
                .is_some_and(|media| media.file_enc_sha256 == encrypted_hash)
        {
            entry.animated = Some(animated);
        }
    }

    pub(super) fn message(&self, id: &str) -> Option<wa::Message> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = entries.get(id)?;
        let media = entry.media.as_ref()?;
        let mut sticker = wa::message::StickerMessage::default();
        sticker.direct_path = Some(media.direct_path.clone());
        sticker.media_key = Some(media.media_key.clone());
        sticker.file_sha256 = Some(media.file_sha256.clone());
        sticker.file_enc_sha256 = Some(media.file_enc_sha256.clone());
        sticker.file_length = (media.file_length != 0).then_some(media.file_length);
        sticker.is_animated = entry.animated;
        sticker.mimetype = Some(media.mime_type.clone());
        let mut message = wa::Message::default();
        if entry.lottie {
            sticker.is_lottie = Some(true);
            let mut inner = wa::Message::default();
            inner.sticker_message = MessageField::some(sticker);
            let mut wrapper = wa::message::FutureProofMessage::default();
            wrapper.message = MessageField::some(inner);
            message.lottie_sticker_message = MessageField::some(wrapper);
        } else {
            message.sticker_message = MessageField::some(sticker);
        }
        Some(message)
    }
}

impl WhatsAppClient {
    pub fn sticker_library(&self, favorites: bool) -> Task<Result<Vec<PickerSticker>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let live = session.lock().await.clone().ok_or("no session yet")?;
            if !favorites {
                use whatsapp_rust::wacore::proto_helpers::MessageExt as _;
                let rows = live
                    .chat_store
                    .own_recent_stickers()
                    .await
                    .map_err(|error| error.to_string())?;
                let mut seen = std::collections::HashSet::new();
                return Ok(rows
                    .into_iter()
                    .filter_map(|row| {
                        let jid = &row.chat_jid;
                        if !(jid.is_group() || jid.is_pn() || jid.is_lid()) {
                            return None;
                        }
                        let message = row.message.as_deref()?;
                        if message.is_view_once() {
                            return None;
                        }
                        let mut media = super::media::media_of(message.get_base_message(), None)?
                            .downloadable?;
                        // The plaintext hash names the picture; the encrypted
                        // one names one upload of it, so two copies of the
                        // same sticker differ in it. Used only where known.
                        let identity = if media.file_sha256.is_empty() {
                            media.content_hash().to_vec()
                        } else {
                            media.file_sha256.clone()
                        };
                        if !seen.insert(identity) {
                            return None;
                        }
                        media.is_thumbnail = false;
                        Some(PickerSticker {
                            id: row.id,
                            source: Some(jid.to_string()),
                            media,
                        })
                    })
                    .take(120)
                    .collect());
            }
            let sync = &live.stickers;
            let mut loaded = sync.loaded.lock().await;
            if !*loaded {
                let report = live
                    .client
                    .resync_app_state([WAPatchName::RegularLow], AppStateResyncMode::Snapshot)
                    .await
                    .map_err(|error| error.to_string())?;
                if !report.synced.contains(&WAPatchName::RegularLow) {
                    return Err("WhatsApp did not provide a sticker sync snapshot".into());
                }
                *loaded = true;
            }
            let entries = sync
                .entries
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let mut items = entries
                .iter()
                .filter_map(|(id, entry)| {
                    entry
                        .media
                        .as_ref()
                        .map(|media| (id, entry.timestamp, media))
                })
                .collect::<Vec<_>>();
            items.sort_unstable_by(|left, right| {
                right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0))
            });
            Ok(items
                .into_iter()
                .take(256)
                .map(|(id, _, media)| PickerSticker {
                    id: id.clone(),
                    source: None,
                    media: media.clone(),
                })
                .collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use whatsapp_rust::wacore::types::events::FavoriteStickerUpdate;

    #[test]
    fn favorites_keep_the_latest_account_event() {
        let sync = StickerSync::default();
        let hash = STANDARD.encode([3; 32]);
        let event = |favorite, timestamp| {
            let mut action = wa::sync_action_value::StickerAction::default();
            action.is_favorite = Some(favorite);
            action.direct_path = Some("/media/sticker".into());
            action.media_key = Some(vec![5; 32]);
            action.file_enc_sha256 = Some(vec![7; 32]);
            action.file_length = Some(42);
            Arc::new(Event::FavoriteStickerUpdate(
                FavoriteStickerUpdate::builder()
                    .filehash(hash.clone())
                    .timestamp(whatsapp_rust::wacore::time::from_millis_or_now(timestamp))
                    .action(Box::new(action))
                    .from_full_sync(true)
                    .build(),
            ))
        };

        sync.handle_event(event(true, 1_700_000_000_000));
        let message = sync.message(&hash).expect("synced favorite");
        let sticker = message.sticker_message.as_option().unwrap();
        assert_eq!(sticker.file_sha256.as_deref(), Some([3; 32].as_slice()));
        sync.remember_animation(&hash, &[9; 32], true);
        assert_eq!(
            sync.message(&hash).unwrap().sticker_message.is_animated,
            None
        );
        sync.remember_animation(&hash, &[7; 32], true);
        sync.handle_event(event(true, 1_700_000_000_000));
        assert_eq!(
            sync.message(&hash).unwrap().sticker_message.is_animated,
            Some(true)
        );
        sync.handle_event(event(false, 1_700_000_000_001));
        sync.handle_event(event(true, 1_700_000_000_000));
        assert!(sync.message(&hash).is_none());
    }
}

/// Whose status updates this account has muted, as the phone said so.
///
/// Learned from the `userStatusMute` app-state action, which arrives with a
/// snapshot of the collection that carries it. Kept here rather than in the
/// store: it is a short list, and the next snapshot replays it.
#[derive(Default)]
pub(super) struct StatusMuteSync {
    muted: Mutex<BTreeMap<String, bool>>,
    loaded: tokio::sync::Mutex<bool>,
}

impl EventHandler for StatusMuteSync {
    fn interest(&self) -> EventInterest {
        EventInterest::of(&[EventKind::UserStatusMuteUpdate])
    }

    fn handle_event(&self, event: Arc<Event>) {
        let Event::UserStatusMuteUpdate(update) = &*event else {
            return;
        };
        self.muted
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(update.jid.to_non_ad_string(), update.muted);
    }
}

impl WhatsAppClient {
    /// The addresses whose status updates are muted, in both of their
    /// identities where the mapping is known: a status arrives from a LID and
    /// a mute may have been made on a phone number.
    pub fn status_mutes(&self) -> Task<Result<Vec<String>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let live = session.lock().await.clone().ok_or("no session yet")?;
            let sync = &live.status_mutes;
            {
                let mut loaded = sync.loaded.lock().await;
                if !*loaded {
                    let report = live
                        .client
                        .resync_app_state([WAPatchName::RegularHigh], AppStateResyncMode::Snapshot)
                        .await
                        .map_err(|error| error.to_string())?;
                    if !report.synced.contains(&WAPatchName::RegularHigh) {
                        return Err("WhatsApp did not provide the status mutes".into());
                    }
                    *loaded = true;
                }
            }
            let muted: Vec<String> = sync
                .muted
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .iter()
                .filter(|(_, muted)| **muted)
                .map(|(jid, _)| jid.clone())
                .collect();
            let mut all = std::collections::BTreeSet::new();
            for jid in muted {
                if let Ok(parsed) = jid.parse::<whatsapp_rust::wacore_binary::jid::Jid>()
                    && let Ok(Some(entry)) = live.client.get_lid_pn_entry(&parsed).await
                {
                    all.insert(format!("{}@lid", entry.lid));
                    all.insert(format!("{}@s.whatsapp.net", entry.phone_number));
                }
                all.insert(jid);
            }
            log::info!("{} addresses have their status updates muted", all.len());
            Ok(all.into_iter().collect())
        })
    }
}
