//! Playback observations belong to the captured account session, never a successor.

use super::*;
use oxidezap_chat_store::{MessageKind, StoredMessage};
use whatsapp_rust::{MessageId, MessageRef};

pub(super) fn receipt_for_ui(receipt: &ReceiptType) -> Option<ReceiptType> {
    match receipt {
        ReceiptType::Delivered => Some(ReceiptType::Delivered),
        ReceiptType::Read | ReceiptType::ReadSelf => Some(ReceiptType::Read),
        ReceiptType::Played => Some(ReceiptType::Played),
        ReceiptType::PlayedSelf => Some(ReceiptType::PlayedSelf),
        _ => None,
    }
}

fn eligible(message: &StoredMessage) -> bool {
    if message.from_me || message.revoked {
        return false;
    }
    match message.kind {
        MessageKind::VoiceNote => true,
        MessageKind::Audio => {
            message.chat_jid.is_newsletter()
                || message
                    .message
                    .as_deref()
                    .is_some_and(MessageExt::is_view_once)
                || message
                    .message
                    .as_deref()
                    .map(oxidezap_chat_store::normalized_message)
                    .and_then(|m| m.audio_message.as_option())
                    .and_then(|m| m.context_info.as_option())
                    .is_some_and(|c| c.forwarded_newsletter_message_info.is_set())
        }
        _ => false,
    }
}

impl WhatsAppClient {
    /// Offline plays are durable local observations, with no deferred network job.
    /// A later real play can retry a failed transport send. Success here means
    /// transport submission, not server acknowledgement or another device updating.
    pub fn mark_audio_played(
        &self,
        chat: String,
        id: String,
        send_allowed: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Task<Result<(), String>> {
        // Capture now: a task first polled after teardown must not select a new session.
        let live = self.session.try_lock().ok().and_then(|slot| slot.clone());
        let gate = self.played_gate.clone();
        let send = send_allowed();
        self.exec.spawn(async move {
            let live = live.ok_or("No account session")?;
            let chat: Jid = chat.parse().map_err(|e| format!("Invalid chat: {e}"))?;
            let message = live
                .chat_store
                .message(&chat, &id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("Audio message is not stored")?;
            if !eligible(&message) {
                return Ok(());
            }
            let _guard = InFlight::claim(gate, message.seq)?;
            observe_playback(
                &live.chat_store,
                &message,
                send.then_some(|| async {
                    let chat: Jid = live
                        .names
                        .chat_key(&live.client, &message.chat_jid)
                        .await
                        .ok_or("Could not resolve the receipt's PN/LID address")?
                        .parse()
                        .map_err(|e| format!("Invalid receipt chat: {e}"))?;
                    let participant = participant_keyed_chat(&chat).then_some(&message.sender_jid);
                    if !send_allowed() {
                        return Err("WhatsApp went offline; playback was recorded locally".into());
                    }
                    send_played(&live.client, &chat, participant, &message.id).await
                }),
            )
            .await
        })
    }
}

async fn send_played(
    client: &Client,
    chat: &Jid,
    participant: Option<&Jid>,
    id: &str,
) -> Result<(), String> {
    // MessageRef accepts user/group chats. The existing raw API also supports newsletters.
    if chat.is_newsletter() {
        return client
            .mark_as_played(chat, None, &[id])
            .await
            .map_err(|e| e.to_string());
    }
    let target = MessageRef::new(
        chat,
        MessageId::new(id).map_err(|e| e.to_string())?,
        participant,
        false,
    )
    .map_err(|e| e.to_string())?;
    client
        .mark_message_played(&target)
        .await
        .map_err(|e| e.to_string())
}

struct InFlight {
    keys: Arc<std::sync::Mutex<HashSet<i64>>>,
    row: i64,
}
impl InFlight {
    fn claim(keys: Arc<std::sync::Mutex<HashSet<i64>>>, row: i64) -> Result<Self, String> {
        let inserted = keys
            .lock()
            .map_err(|_| "Playback tracker unavailable")?
            .insert(row);
        if !inserted {
            return Err("Playback observation already in flight".into());
        }
        Ok(Self { keys, row })
    }
}
impl Drop for InFlight {
    fn drop(&mut self) {
        if let Ok(mut keys) = self.keys.lock() {
            keys.remove(&self.row);
        }
    }
}

async fn observe_playback<F, Fut>(
    store: &ChatStore,
    message: &StoredMessage,
    send: Option<F>,
) -> Result<(), String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    if !eligible(message) {
        return Ok(());
    }
    store
        .record_audio_played(message, false)
        .map_err(|e| e.to_string())?;
    store.flush().await.map_err(|e| e.to_string())?;
    let Some(send) = send else {
        return Ok(());
    };
    if store
        .audio_played_receipt_sent(message)
        .await
        .map_err(|e| e.to_string())?
    {
        return Ok(());
    }
    send().await?;
    store
        .record_audio_played(message, true)
        .map_err(|e| e.to_string())?;
    store.flush().await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use whatsapp_rust::wacore::types::events::InboundMessage;
    use whatsapp_rust::wacore::types::message::{MessageInfo, MessageSource};

    #[test]
    fn linked_play_is_not_reinterpreted_as_a_recipient_play() {
        assert_eq!(
            receipt_for_ui(&ReceiptType::PlayedSelf),
            Some(ReceiptType::PlayedSelf)
        );
        assert_eq!(
            receipt_for_ui(&ReceiptType::Played),
            Some(ReceiptType::Played)
        );
    }

    async fn fixture(group: bool, ptt: bool) -> (SqliteStore, Arc<ChatStore>, StoredMessage) {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let db = SqliteStore::open(&format!(
            "file:audio_played_{}_{}?mode=memory&cache=shared",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
        .await
        .unwrap();
        db.database()
            .provision_device(db.device_id())
            .await
            .unwrap();
        let store = ChatStore::new(&db).await.unwrap();
        let chat: Jid = if group {
            "120363000000000001@g.us"
        } else {
            "559900000001@s.whatsapp.net"
        }
        .parse()
        .unwrap();
        let sender: Jid = "111000011112222@lid".parse().unwrap();
        let mut audio = wa::message::AudioMessage::default();
        audio.ptt = Some(ptt);
        let mut proto = wa::Message::default();
        proto.audio_message = whatsapp_rust::buffa::MessageField::some(audio);
        let info = MessageInfo {
            source: MessageSource {
                chat: chat.clone(),
                sender,
                is_group: group,
                ..Default::default()
            },
            id: "AUDIO-SYNTHETIC".into(),
            ..Default::default()
        };
        store
            .commit_inbound_batch(&[InboundMessage::builder()
                .message(Arc::new(proto))
                .info(Arc::new(info))
                .build()])
            .await
            .unwrap();
        let message = store
            .message(&chat, "AUDIO-SYNTHETIC")
            .await
            .unwrap()
            .unwrap();
        (db, store, message)
    }

    #[tokio::test]
    async fn actual_observation_retries_failure_but_never_repeats_success() {
        let (_db, store, message) = fixture(false, true).await;
        let mut calls = 0;
        let failed = observe_playback(
            &store,
            &message,
            Some(|| {
                calls += 1;
                std::future::ready(Err("transport failed".into()))
            }),
        )
        .await;
        assert!(failed.is_err());
        assert!(!store.audio_played_receipt_sent(&message).await.unwrap());
        for _ in 0..3 {
            observe_playback(
                &store,
                &message,
                Some(|| {
                    calls += 1;
                    std::future::ready(Ok(()))
                }),
            )
            .await
            .unwrap();
        }
        assert_eq!(calls, 2);
        assert_eq!(
            store
                .message(&message.chat_jid, &message.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            oxidezap_chat_store::MessageStatus::Played
        );
    }

    #[tokio::test]
    async fn offline_play_is_local_and_other_account_never_inherits_submission() {
        let (_db, store, message) = fixture(false, true).await;
        observe_playback(
            &store,
            &message,
            None::<fn() -> std::future::Ready<Result<(), String>>>,
        )
        .await
        .unwrap();
        assert!(!store.audio_played_receipt_sent(&message).await.unwrap());
        assert_eq!(
            store
                .message(&message.chat_jid, &message.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            oxidezap_chat_store::MessageStatus::Played
        );
        let (_other_db, other_store, other_message) = fixture(false, true).await;
        assert_ne!(
            other_store
                .message(&other_message.chat_jid, &other_message.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            oxidezap_chat_store::MessageStatus::Played
        );
    }

    #[tokio::test]
    async fn own_and_ordinary_audio_do_not_emit_receipts() {
        let (_db, store, mut message) = fixture(false, true).await;
        message.from_me = true;
        observe_playback(
            &store,
            &message,
            Some(|| async { panic!("own audio receipt") }),
        )
        .await
        .unwrap();
        let (_db2, store, message) = fixture(false, false).await;
        observe_playback(
            &store,
            &message,
            Some(|| async { panic!("ordinary audio receipt") }),
        )
        .await
        .unwrap();
        assert_ne!(
            store
                .message(&message.chat_jid, &message.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            oxidezap_chat_store::MessageStatus::Played
        );
    }

    #[tokio::test]
    async fn newsletter_origin_and_view_once_audio_follow_the_official_eligibility_guards() {
        let (_db, _store, mut message) = fixture(false, false).await;
        assert!(!eligible(&message));
        let mut proto = message.message.take().unwrap();
        proto.audio_message.as_option_mut().unwrap().view_once = Some(true);
        message.message = Some(proto);
        assert!(eligible(&message));
        let mut proto = message.message.take().unwrap();
        let audio = proto.audio_message.as_option_mut().unwrap();
        audio.view_once = None;
        let mut context = wa::ContextInfo::default();
        context.forwarded_newsletter_message_info =
            whatsapp_rust::buffa::MessageField::some(Default::default());
        audio.context_info = whatsapp_rust::buffa::MessageField::some(context);
        message.message = Some(proto);
        assert!(eligible(&message));
        message.from_me = true;
        assert!(!eligible(&message));
        message.from_me = false;
        message.message = None;
        message.chat_jid = "120363000000000002@newsletter".parse().unwrap();
        assert!(eligible(&message));
        message.revoked = true;
        assert!(!eligible(&message));
    }

    #[tokio::test]
    async fn group_uses_original_participant_and_direct_drops_it() {
        for group in [false, true] {
            let (_db, _store, message) = fixture(group, true).await;
            let id = MessageId::new(message.id.clone()).unwrap();
            let participant =
                participant_keyed_chat(&message.chat_jid).then_some(&message.sender_jid);
            let target = MessageRef::new(&message.chat_jid, id, participant, false).unwrap();
            assert_eq!(
                target.receipt_sender(),
                group.then_some(&message.sender_jid)
            );
            assert_eq!(target.chat(), &message.chat_jid);
        }
    }

    #[test]
    fn multiple_windows_coalesce_and_failure_releases_the_key() {
        let keys = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let first = InFlight::claim(keys.clone(), 1).unwrap();
        assert!(InFlight::claim(keys.clone(), 1).is_err());
        assert!(InFlight::claim(keys.clone(), 2).is_ok());
        drop(first);
        assert!(InFlight::claim(keys, 1).is_ok());
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod wire_tests {
    use super::*;
    use whatsapp_rust::client::NodeFilter;
    use whatsapp_rust::store::persistence_manager::PersistenceManager;
    use whatsapp_rust::wacore::net::{
        HttpClient, HttpRequest, HttpResponse, Transport, TransportEvent, TransportFactory,
    };

    struct NoNetwork;
    #[async_trait::async_trait]
    impl TransportFactory for NoNetwork {
        async fn create_transport(
            &self,
        ) -> anyhow::Result<(Arc<dyn Transport>, async_channel::Receiver<TransportEvent>)> {
            panic!("a played receipt probe must never connect")
        }
    }
    #[async_trait::async_trait]
    impl HttpClient for NoNetwork {
        async fn execute(&self, _: HttpRequest) -> anyhow::Result<HttpResponse> {
            panic!("a played receipt probe must never use HTTP")
        }
    }

    // Observe the real core builder before the deliberately absent socket rejects
    // transmission. This proves wire construction, not server/device delivery.
    #[tokio::test]
    async fn core_played_wire_matches_preserved_bundle_privacy_and_addressing() {
        for (index, (chat, disabled, expected, participant)) in [
            ("559900000001@s.whatsapp.net", false, "played", false),
            ("111000011112222@lid", false, "played", false),
            ("111000011112222@lid", true, "played-self", false),
            ("120363000000000001@g.us", true, "played", true),
            ("120363000000000001@g.us", false, "played", true),
            ("120363000000000002@newsletter", false, "played-self", false),
        ]
        .into_iter()
        .enumerate()
        {
            let db = SqliteStore::open(&format!(
                "file:played_wire_{}_{}?mode=memory&cache=shared",
                std::process::id(),
                index
            ))
            .await
            .unwrap();
            let pm = Arc::new(PersistenceManager::new(Arc::new(db)).await.unwrap());
            pm.modify_device(|device| device.read_receipts_disabled = disabled)
                .await;
            let (client, _events) = Client::builder()
                .with_runtime(whatsapp_rust::TokioRuntime)
                .with_persistence_manager(pm)
                .with_transport_factory(NoNetwork)
                .with_http_client(NoNetwork)
                .build()
                .await
                .unwrap()
                .into_parts();
            let waiter = client.wait_for_sent_node(NodeFilter::tag("receipt"));
            let chat: Jid = chat.parse().unwrap();
            let sender: Jid = "111000011113333@lid".parse().unwrap();
            assert!(
                send_played(
                    &client,
                    &chat,
                    participant.then_some(&sender),
                    "SYNTHETIC-AUDIO"
                )
                .await
                .is_err()
            );
            let node = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
                .await
                .expect("core constructed the receipt before rejecting the absent socket")
                .unwrap();
            let attr = |name| node.attrs.get(name).map(|v| v.as_str().to_string());
            assert_eq!(attr("type").as_deref(), Some(expected));
            assert_eq!(attr("to"), Some(chat.to_string()));
            assert_eq!(attr("id").as_deref(), Some("SYNTHETIC-AUDIO"));
            assert_eq!(attr("participant"), participant.then(|| sender.to_string()));
            assert!(attr("t").is_some());
        }
    }
}
