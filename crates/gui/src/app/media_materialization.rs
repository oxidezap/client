//! Cache-only hydration owned by the surface that is actually drawn.
//!
//! Native frames keep their references. The list reports its viewport after
//! layout; Status and the viewer report one row. A single background read is
//! admitted at a time, including reads that have lost their owner. Nothing in
//! this path asks WhatsApp for a missing payload.

use super::*;
use oxidezap_core::STATUS_BROADCAST_JID;
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_DEMAND: usize = 64;
const MAX_READ_BYTES: usize = oxidezap_core::WEB_MEDIA_BUDGET_BYTES as usize;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Owner {
    Conversation(String),
    Status(String),
    Viewer(String, String),
}

#[derive(Clone, PartialEq, Eq)]
struct Target {
    owner: Owner,
    generation: u64,
    account: (Option<String>, Option<String>),
    jid: String,
    id: String,
    content: String,
    edited: bool,
    expected: MediaContent,
    key: String,
    preview: bool,
}

impl Target {
    fn reference(message: &ChatMessage) -> Option<(&String, bool)> {
        let media = message.media.as_ref()?;
        if message.revoked || (!media.data.is_empty() && !media.data_is_preview) {
            return None;
        }
        media
            .cache_key
            .as_ref()
            .map(|key| (key, false))
            .or_else(|| {
                media.data.is_empty().then_some(())?;
                media.preview_cache_key.as_ref().map(|key| (key, true))
            })
    }

    fn new(app: &WhatsAppApp, owner: Owner, jid: &str, message: &ChatMessage) -> Option<Self> {
        let (key, preview) = Self::reference(message)?;
        let media = message.media.as_ref()?;
        Some(Self {
            owner,
            generation: app.document_state_generation,
            account: (app.account_jid.clone(), app.account_lid.clone()),
            jid: jid.into(),
            id: message.id.clone(),
            content: message.content.clone(),
            edited: message.edited,
            expected: media.clone(),
            key: key.clone(),
            preview,
        })
    }

    fn matches(&self, message: &ChatMessage) -> bool {
        message.id == self.id
            && !message.revoked
            && message.content == self.content
            && message.edited == self.edited
            && message.media.as_ref() == Some(&self.expected)
    }
}

struct Running {
    ticket: u64,
    target: Target,
    cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
pub(super) struct Materialization {
    owner: Option<Owner>,
    desired: Vec<Target>,
    failed: Vec<Target>,
    running: Option<Running>,
    ticket: u64,
}

impl Drop for Materialization {
    fn drop(&mut self) {
        if let Some(running) = &self.running {
            running.cancelled.store(true, Ordering::Release);
        }
    }
}

impl Materialization {
    fn surface(&mut self, owner: Option<Owner>) {
        if self.owner == owner {
            return;
        }
        self.owner = owner;
        self.set_desired(Vec::new());
    }

    fn set_desired(&mut self, desired: Vec<Target>) {
        self.desired = desired.into_iter().take(MAX_DEMAND).collect();
        self.failed.retain(|target| self.desired.contains(target));
        if let Some(running) = &self.running
            && !self.desired.contains(&running.target)
        {
            running.cancelled.store(true, Ordering::Release);
        }
    }

    fn start(&mut self) -> Option<(u64, Target, Arc<AtomicBool>)> {
        if self.running.is_some() {
            return None;
        }
        let target = self
            .desired
            .iter()
            .find(|target| !self.failed.contains(target))?
            .clone();
        self.ticket = self.ticket.wrapping_add(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        self.running = Some(Running {
            ticket: self.ticket,
            target: target.clone(),
            cancelled: Arc::clone(&cancelled),
        });
        Some((self.ticket, target, cancelled))
    }

    fn finish(&mut self, ticket: u64) -> Option<Target> {
        if self.running.as_ref()?.ticket != ticket {
            return None;
        }
        let running = self.running.take()?;
        let wanted =
            self.desired.contains(&running.target) && !running.cancelled.load(Ordering::Acquire);
        // Failed attempts must not retry on every repaint. Keep only attempts
        // still owned by the current viewport; leaving and returning can retry.
        if wanted {
            self.failed.push(running.target.clone());
            Some(running.target)
        } else {
            None
        }
    }
}

impl WhatsAppApp {
    pub fn has_deferred_media_reference(&self, message: &ChatMessage) -> bool {
        Target::reference(message).is_some()
            && self
                .media_cache()
                .is_some_and(|cache| cache.defers_message_media())
    }

    /// Let cache-only hydration finish before automatic network improvements.
    pub fn awaiting_cached_media(&self, jid: &str, message: &ChatMessage) -> bool {
        self.has_deferred_media_reference(message)
            && !self.media_materialization.failed.iter().any(|target| {
                target.generation == self.document_state_generation
                    && target.account.0 == self.account_jid
                    && target.account.1 == self.account_lid
                    && target.jid == jid
                    && target.matches(message)
            })
    }

    fn materialization_owner(&self, cx: &App) -> Option<Owner> {
        if self.showing_settings(cx)
            || self.paste_preview_showing()
            || !matches!(
                self.app_state,
                AppState::Connected | AppState::Offline | AppState::OfflineLoggedOut { .. }
            )
        {
            return None;
        }
        let viewer = self.viewer.read(cx);
        if let (Some(jid), Some(id)) = (viewer.jid(), viewer.current_id()) {
            return Some(Owner::Viewer(jid.into(), id.into()));
        }
        if self.destination == Destination::Status {
            return self.status_pane.shown().map(|id| Owner::Status(id.into()));
        }
        self.visible_chat.clone().map(Owner::Conversation)
    }

    pub(super) fn reconcile_media_owner(&mut self, cx: &App) {
        let owner = self
            .media_cache()
            .filter(|cache| cache.defers_message_media())
            .and_then(|_| self.materialization_owner(cx));
        self.media_materialization.surface(owner);
    }

    /// Called after the connected view has reconciled its visible surfaces.
    pub fn note_media_surface(&mut self, cx: &mut Context<Self>) {
        self.reconcile_media_owner(cx);
        let owner = self.media_materialization.owner.clone();
        if owner.is_none() {
            return;
        }
        let row = match &owner {
            Some(Owner::Status(id)) => Some((STATUS_BROADCAST_JID.to_string(), id.clone())),
            Some(Owner::Viewer(jid, id)) => Some((jid.clone(), id.clone())),
            _ => None,
        };
        if let Some((jid, id)) = row {
            let targets = self
                .chat_named(&jid)
                .and_then(|chat| chat.messages.iter().find(|m| m.id == id))
                .and_then(|message| {
                    Target::new(
                        self,
                        owner.clone().expect("row has an owner"),
                        &jid,
                        message,
                    )
                })
                .into_iter()
                .collect();
            self.media_materialization.set_desired(targets);
        }
        if !matches!(owner, Some(Owner::Conversation(_))) {
            self.pump_materialization(cx);
        }
    }

    /// The list reports only rows intersecting its viewport, excluding overdraw.
    pub fn note_visible_media(&mut self, jid: String, ids: Vec<String>, cx: &mut Context<Self>) {
        let owner = self.materialization_owner(cx);
        if owner != Some(Owner::Conversation(jid.clone())) {
            return;
        }
        self.media_materialization.surface(owner.clone());
        let targets = self
            .chat_named(&jid)
            .into_iter()
            .flat_map(|chat| {
                ids.iter()
                    .take(MAX_DEMAND)
                    .filter_map(|id| chat.messages.iter().find(|m| &m.id == id))
            })
            .filter_map(|message| {
                Target::new(
                    self,
                    owner.clone().expect("conversation owner checked"),
                    &jid,
                    message,
                )
            })
            .collect();
        self.media_materialization.set_desired(targets);
        self.pump_materialization(cx);
    }

    fn target_is_current(&self, target: &Target, cx: &App) -> bool {
        target.generation == self.document_state_generation
            && target.account == (self.account_jid.clone(), self.account_lid.clone())
            && self.materialization_owner(cx).as_ref() == Some(&target.owner)
            && self
                .chat_named(&target.jid)
                .and_then(|chat| chat.messages.iter().find(|m| m.id == target.id))
                .is_some_and(|message| target.matches(message))
    }

    fn pump_materialization(&mut self, cx: &mut Context<Self>) {
        self.reconcile_media_owner(cx);
        let Some(cache) = self
            .media_cache()
            .filter(|cache| cache.defers_message_media())
        else {
            return;
        };
        // A receipt may have replaced a row while its read was pending. Do not
        // dispatch stale queued descriptors merely because their IDs survive.
        let desired = self
            .media_materialization
            .desired
            .iter()
            .filter(|target| self.target_is_current(target, cx))
            .cloned()
            .collect();
        self.media_materialization.set_desired(desired);
        let Some((ticket, target, cancelled)) = self.media_materialization.start() else {
            return;
        };
        let key = target.key.clone();
        let read = cx.background_spawn(async move {
            if cancelled.load(Ordering::Acquire) {
                return Err("materialization lost its owner".to_string());
            }
            cache.read_deferred(&key, MAX_READ_BYTES)
        });
        cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            let result = read.await;
            let _ = entity.update(cx, |app, cx| {
                app.reconcile_media_owner(cx);
                if let Some(target) = app.media_materialization.finish(ticket)
                    && app.target_is_current(&target, cx)
                {
                    if let Ok(bytes) = result {
                        // One content-addressed payload can back several visible
                        // rows, including a full and a preview reference. Adopt
                        // metadata separately, but share the same byte buffer.
                        let recipients: Vec<_> = app
                            .media_materialization
                            .desired
                            .iter()
                            .filter(|other| {
                                other.key == target.key && app.target_is_current(other, cx)
                            })
                            .cloned()
                            .collect();
                        for recipient in recipients {
                            let message = app
                                .find_chat_mut(&recipient.jid)
                                .and_then(|chat| {
                                    chat.messages.iter_mut().find(|m| m.id == recipient.id)
                                })
                                .expect("recipient checked immediately above");
                            let media = message
                                .media
                                .as_mut()
                                .expect("recipient checked immediately above");
                            if recipient.preview {
                                media.preview_cache_key = None;
                                media.data = Arc::clone(&bytes);
                            } else {
                                media.cache_key = None;
                                media.adopt_full_bytes(Arc::clone(&bytes));
                            }
                            app.decoded_images.borrow_mut().shift_remove(&recipient.id);
                            app.sticker_validation
                                .borrow_mut()
                                .shift_remove(&recipient.id);
                        }
                        app.invalidate_message_cache(&target.jid, cx);
                        app.sweep_retained_media_keeping(Some(&target.id), cx);
                    }
                    // Opening Status is also the request to fetch full media.
                    // Resume its existing behavior after the cache-only attempt.
                    if matches!(target.owner, Owner::Status(_)) {
                        app.fetch_shown_status_after_cache(cx);
                    }
                    cx.notify();
                }
                app.pump_materialization(cx);
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidezap_core::MediaType;
    use std::sync::atomic::AtomicUsize;

    struct Cache {
        reads: AtomicUsize,
        bytes: AtomicUsize,
        fail: AtomicBool,
        payload: Vec<u8>,
    }

    impl Default for Cache {
        fn default() -> Self {
            let mut payload = Vec::new();
            image::DynamicImage::new_rgb8(1, 1)
                .write_to(
                    &mut std::io::Cursor::new(&mut payload),
                    image::ImageFormat::Png,
                )
                .unwrap();
            payload.resize(1024 * 1024, 0);
            Self {
                payload,
                reads: AtomicUsize::new(0),
                bytes: AtomicUsize::new(0),
                fail: AtomicBool::new(false),
            }
        }
    }

    impl crate::session::MediaCache for Cache {
        fn read(&self, _: &str) -> Result<Arc<Vec<u8>>, String> {
            panic!("a deferred payload must not use eager read")
        }
        fn defers_message_media(&self) -> bool {
            true
        }
        fn read_deferred(&self, _: &str, most: usize) -> Result<Arc<Vec<u8>>, String> {
            assert_eq!(most, MAX_READ_BYTES);
            self.reads.fetch_add(1, Ordering::Relaxed);
            if self.fail.load(Ordering::Relaxed) {
                return Err("missing".into());
            }
            self.bytes.fetch_add(1024 * 1024, Ordering::Relaxed);
            Ok(Arc::new(self.payload.clone()))
        }
        fn stage(&self, _: &str, _: &[u8]) -> Result<(), String> {
            unreachable!()
        }
        fn discard(&self, _: &str) {
            unreachable!()
        }
    }

    fn message(n: usize) -> ChatMessage {
        let mut message = ChatMessage::new_incoming(
            format!("m{n}"),
            "peer@example.invalid".into(),
            "photo".into(),
        );
        let mut media = MediaContent::image(Arc::new(Vec::new()), "image/jpeg".into(), true);
        media.preview_cache_key = Some(format!("preview-{n}"));
        message.media = Some(media);
        message
    }

    #[gpui::test]
    fn history_preserved_preview_does_not_wait_for_an_ineligible_cache_read(
        cx: &mut gpui::TestAppContext,
    ) {
        let (app, cache) = setup(cx);
        app.update(cx, |app, cx| {
            let chat = Arc::make_mut(&mut app.chats[0]);
            let old = chat.messages[0].media.as_mut().unwrap();
            old.data = Arc::new(vec![1, 2, 3]);
            old.data_is_preview = true;
            old.preview_cache_key = None;
            chat.insert_history_message(message(0));
            let row = chat.messages.iter().find(|m| m.id == "m0").unwrap().clone();
            let media = row.media.as_ref().unwrap();
            assert!(!media.data.is_empty() && media.preview_cache_key.is_some());
            assert!(!app.has_deferred_media_reference(&row));
            assert!(!app.awaiting_cached_media("peer@example.invalid", &row));
            app.visible_chat = Some("peer@example.invalid".into());
            app.note_visible_media("peer@example.invalid".into(), vec!["m0".into()], cx);
        });
        cx.executor().run_until_parked();
        assert_eq!(cache.reads.load(Ordering::Relaxed), 0);
    }

    #[gpui::test]
    fn reopened_status_skips_materialized_preview_but_waits_for_full_cache_attempt(
        cx: &mut gpui::TestAppContext,
    ) {
        let (app, cache) = setup(cx);
        for media_type in [MediaType::Image, MediaType::Video] {
            app.update(cx, |app, cx| {
                app.media_materialization.surface(None);
                let chat = Arc::make_mut(&mut app.chats[0]);
                chat.jid = STATUS_BROADCAST_JID.into();
                chat.is_status = true;
                chat.messages = vec![message(0)];
                let media = chat.messages[0].media.as_mut().unwrap();
                media.data = Arc::new(vec![1, 2, 3]);
                media.data_is_preview = true;
                let mut incoming = message(0);
                incoming.media.as_mut().unwrap().media_type = media_type;
                chat.insert_history_message(incoming);
                app.destination = Destination::Status;
                app.status_pane.open("peer@example.invalid".into());
                app.status_pane.follow(Some("m0".into()));
                assert!(!app.shown_status_awaits_cache());
                app.note_media_surface(cx);
            });
            cx.executor().run_until_parked();
            let reads = cache.reads.load(Ordering::Relaxed);
            app.update(cx, |app, cx| {
                let media = Arc::make_mut(&mut app.chats[0]).messages[0]
                    .media
                    .as_mut()
                    .unwrap();
                media.cache_key = Some("missing-full".into());
                assert!(app.shown_status_awaits_cache());
                cache.fail.store(true, Ordering::Relaxed);
                app.note_media_surface(cx);
            });
            cx.executor().run_until_parked();
            assert_eq!(cache.reads.load(Ordering::Relaxed), reads + 1);
            app.read_with(cx, |app, _| {
                assert!(!app.shown_status_awaits_cache());
                assert_eq!(
                    app.chats[0].messages[0]
                        .media
                        .as_ref()
                        .unwrap()
                        .data
                        .as_slice(),
                    &[1, 2, 3]
                );
            });
        }
    }

    fn setup(cx: &mut gpui::TestAppContext) -> (Entity<WhatsAppApp>, Arc<Cache>) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
        });
        let cache = Arc::new(Cache::default());
        let app = cx.update(|cx| cx.new(WhatsAppApp::new));
        app.update(cx, |app, _| {
            app.app_state = AppState::Offline;
            app.materialization_cache = Some(cache.clone());
            let mut chat = Chat::new("peer@example.invalid".into());
            chat.messages = (0..12).map(message).collect();
            app.chats = vec![Arc::new(chat)];
        });
        (app, cache)
    }

    #[gpui::test]
    fn no_selection_reads_nothing_then_only_visible_inline_previews_load_offline(
        cx: &mut gpui::TestAppContext,
    ) {
        let (app, cache) = setup(cx);
        app.update(cx, |app, cx| app.note_media_surface(cx));
        cx.executor().run_until_parked();
        assert_eq!(cache.reads.load(Ordering::Relaxed), 0);
        app.update(cx, |app, cx| {
            app.visible_chat = Some("peer@example.invalid".into());
            app.note_visible_media(
                "peer@example.invalid".into(),
                vec!["m4".into(), "m5".into()],
                cx,
            );
        });
        cx.executor().run_until_parked();
        assert_eq!(cache.reads.load(Ordering::Relaxed), 2);
        assert_eq!(cache.bytes.load(Ordering::Relaxed), 2 * 1024 * 1024);
        app.read_with(cx, |app, _| {
            for (n, message) in app.chats[0].messages.iter().enumerate() {
                let media = message.media.as_ref().unwrap();
                if [4, 5].contains(&n) {
                    assert_eq!(media.data.len(), 1024 * 1024);
                    assert!(media.data_is_preview);
                    assert_eq!(media.mime_type, "image/jpeg");
                    assert!(media.downloadable.is_none());
                    assert!(media.preview_cache_key.is_none());
                } else {
                    assert!(media.data.is_empty());
                    assert!(media.preview_cache_key.is_some());
                }
            }
        });
    }

    #[gpui::test]
    fn stale_reads_cannot_restore_revoked_edited_replaced_or_other_account_media(
        cx: &mut gpui::TestAppContext,
    ) {
        let (app, cache) = setup(cx);
        for change in 0..7 {
            app.update(cx, |app, cx| {
                Arc::make_mut(&mut app.chats[0]).messages[0] = message(0);
                app.visible_chat = Some("peer@example.invalid".into());
                app.note_visible_media("peer@example.invalid".into(), vec!["m0".into()], cx);
                match change {
                    0 => app.document_state_generation += 1,
                    1 => app.account_jid = Some("other@example.invalid".into()),
                    2 => Arc::make_mut(&mut app.chats[0]).messages[0].revoked = true,
                    3 => {
                        Arc::make_mut(&mut app.chats[0]).messages[0].content =
                            "edited caption".into()
                    }
                    4 => {
                        Arc::make_mut(&mut app.chats[0]).messages[0]
                            .media
                            .as_mut()
                            .unwrap()
                            .preview_cache_key = Some("replaced".into())
                    }
                    5 => app.visible_chat = None,
                    6 => {
                        // A temporary departure invalidates even a later identical owner.
                        app.media_materialization.surface(None);
                        app.media_materialization
                            .surface(Some(Owner::Conversation("peer@example.invalid".into())));
                        app.note_visible_media(
                            "peer@example.invalid".into(),
                            vec!["m0".into()],
                            cx,
                        );
                        // Remove demand again to leave this attempt cancelled.
                        app.media_materialization.set_desired(Vec::new());
                    }
                    _ => unreachable!(),
                }
            });
            cx.executor().run_until_parked();
            app.read_with(cx, |app, _| {
                assert!(
                    app.chats[0].messages[0]
                        .media
                        .as_ref()
                        .unwrap()
                        .data
                        .is_empty()
                )
            });
        }
        assert!(cache.reads.load(Ordering::Relaxed) <= 7);
    }

    #[gpui::test]
    fn failures_do_not_retry_each_repaint_but_returning_to_the_row_can_retry(
        cx: &mut gpui::TestAppContext,
    ) {
        let (app, cache) = setup(cx);
        cache.fail.store(true, Ordering::Relaxed);
        for _ in 0..5 {
            app.update(cx, |app, cx| {
                app.visible_chat = Some("peer@example.invalid".into());
                app.note_visible_media("peer@example.invalid".into(), vec!["m0".into()], cx);
            });
            cx.executor().run_until_parked();
        }
        assert_eq!(cache.reads.load(Ordering::Relaxed), 1);
        cache.fail.store(false, Ordering::Relaxed);
        app.update(cx, |app, cx| {
            app.visible_chat = None;
            app.note_media_surface(cx);
            app.visible_chat = Some("peer@example.invalid".into());
            app.note_visible_media("peer@example.invalid".into(), vec!["m0".into()], cx);
        });
        cx.executor().run_until_parked();
        assert_eq!(cache.reads.load(Ordering::Relaxed), 2);
    }

    #[gpui::test]
    fn one_read_slot_survives_cancellation_and_demand_is_bounded(cx: &mut gpui::TestAppContext) {
        let (app, _) = setup(cx);
        let target = app.read_with(cx, |app, _| {
            Target::new(
                app,
                Owner::Conversation("peer@example.invalid".into()),
                "peer@example.invalid",
                &app.chats[0].messages[0],
            )
            .unwrap()
        });
        let mut state = Materialization::default();
        state.surface(Some(target.owner.clone()));
        state.set_desired(vec![target.clone(); MAX_DEMAND * 2]);
        assert_eq!(state.desired.len(), MAX_DEMAND);
        let (ticket, _, cancelled) = state.start().unwrap();
        assert!(state.start().is_none());
        state.surface(None);
        assert!(cancelled.load(Ordering::Acquire));
        state.surface(Some(target.owner.clone()));
        state.set_desired(vec![target]);
        assert!(
            state.start().is_none(),
            "cancelled work still occupies the read slot"
        );
        assert!(
            state.finish(ticket).is_none(),
            "returning cannot revive cancelled work"
        );
        assert!(state.start().is_some());
    }
    #[gpui::test]
    fn shown_status_loads_an_offline_preview_without_a_download_descriptor(
        cx: &mut gpui::TestAppContext,
    ) {
        let (app, cache) = setup(cx);
        app.update(cx, |app, cx| {
            let chat = Arc::make_mut(&mut app.chats[0]);
            chat.jid = STATUS_BROADCAST_JID.into();
            chat.is_status = true;
            app.destination = Destination::Status;
            app.status_pane.open("peer@example.invalid".into());
            app.status_pane.follow(Some("m0".into()));
            app.note_media_surface(cx);
        });
        cx.executor().run_until_parked();
        assert_eq!(cache.reads.load(Ordering::Relaxed), 1);
        app.read_with(cx, |app, _| {
            let media = app.chats[0].messages[0].media.as_ref().unwrap();
            assert_eq!(media.data.len(), 1024 * 1024);
            assert!(media.data_is_preview && media.downloadable.is_none());
            assert!(
                app.chats[0].messages[1]
                    .media
                    .as_ref()
                    .unwrap()
                    .data
                    .is_empty()
            );
        });
    }

    #[gpui::test]
    fn full_cache_hydration_uses_the_original_mime_and_cannot_overwrite_an_explicit_download(
        cx: &mut gpui::TestAppContext,
    ) {
        let (app, cache) = setup(cx);
        for race in [false, true] {
            app.update(cx, |app, cx| {
                let message = &mut Arc::make_mut(&mut app.chats[0]).messages[0];
                *message = self::message(0);
                let media = message.media.as_mut().unwrap();
                media.cache_key = Some("full".into());
                media.downloadable = Some(
                    serde_json::from_value(serde_json::json!({
                        "direct_path":"/v/synthetic", "media_key":[], "file_enc_sha256":[],
                        "file_sha256":vec![7;32], "file_length":1024, "mime_type":"image/webp",
                        "duration_secs":null, "download_type":"image"
                    }))
                    .unwrap(),
                );
                app.visible_chat = Some("peer@example.invalid".into());
                app.note_visible_media("peer@example.invalid".into(), vec!["m0".into()], cx);
                if race {
                    let media = Arc::make_mut(&mut app.chats[0]).messages[0]
                        .media
                        .as_mut()
                        .unwrap();
                    media.cache_key = None;
                    media.adopt_full_bytes(Arc::new(vec![9]));
                }
            });
            cx.executor().run_until_parked();
            app.read_with(cx, |app, _| {
                let media = app.chats[0].messages[0].media.as_ref().unwrap();
                assert_eq!(media.mime_type, "image/webp");
                assert!(!media.data_is_preview && !media.data_is_server_preview);
                assert!(media.cache_key.is_none() && media.preview_cache_key.is_none());
                assert_eq!(media.data.len(), if race { 1 } else { 1024 * 1024 });
            });
        }
        assert_eq!(cache.reads.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn actual_bottom_aligned_list_hydrates_visible_rows_and_not_overdraw() {
        use gpui::AppContext as _;
        let mut cx = gpui::HeadlessAppContext::with_asset_source(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("DejaVu Sans")),
            Arc::new(crate::assets::Assets),
        );
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
        });
        let cache = Arc::new(Cache::default());
        let mut app_entity = None;
        let window = cx
            .open_window(gpui::size(gpui::px(1000.), gpui::px(800.)), |window, cx| {
                let app = cx.new(|cx| {
                    let mut app = WhatsAppApp::new(cx);
                    app.app_state = AppState::Offline;
                    app.materialization_cache = Some(cache.clone());
                    let mut chat = Chat::new("peer@example.invalid".into());
                    chat.messages = (0..24)
                        .map(|n| {
                            let mut message = message(n);
                            let media = message.media.as_mut().unwrap();
                            media.mime_type = "image/png".into();
                            media.width = Some(300);
                            media.height = Some(300);
                            message
                        })
                        .collect();
                    app.chats = vec![Arc::new(chat)];
                    app
                });
                app_entity = Some(app.clone());
                cx.new(|cx| gpui_component::Root::new(app, window, cx))
            })
            .unwrap();
        let app = app_entity.unwrap();
        cx.run_until_parked();
        assert_eq!(cache.reads.load(Ordering::Relaxed), 0);
        window
            .update(&mut cx, |_, window, cx| {
                app.update(cx, |app, cx| {
                    app.select_chat(
                        "peer@example.invalid".into(),
                        ChatOpen::ToPreview,
                        window,
                        cx,
                    );
                })
            })
            .unwrap();
        cx.run_until_parked();
        let first = cache.reads.load(Ordering::Relaxed);
        assert!(
            first > 0 && first < 8,
            "expected visible media, not the 24-row history or overdraw: {first}"
        );
        app.read_with(&cx, |app, _| {
            assert!(
                app.chats[0].messages[0]
                    .media
                    .as_ref()
                    .unwrap()
                    .data
                    .is_empty()
            );
            assert!(
                !app.chats[0].messages[23]
                    .media
                    .as_ref()
                    .unwrap()
                    .data
                    .is_empty()
            );
        });
        app.update(&mut cx, |app, cx| {
            app.message_list.scroll_to(gpui::ListOffset {
                item_ix: 0,
                offset_in_item: gpui::px(0.),
            });
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cache.reads.load(Ordering::Relaxed) > first);
        app.read_with(&cx, |app, _| {
            assert!(
                !app.chats[0].messages[0]
                    .media
                    .as_ref()
                    .unwrap()
                    .data
                    .is_empty()
            )
        });
    }
    #[gpui::test]
    fn repeated_visible_keys_share_one_cache_read_and_one_buffer(cx: &mut gpui::TestAppContext) {
        let (app, cache) = setup(cx);
        app.update(cx, |app, cx| {
            let chat = Arc::make_mut(&mut app.chats[0]);
            chat.messages[1].media.as_mut().unwrap().preview_cache_key = Some("preview-0".into());
            app.visible_chat = Some("peer@example.invalid".into());
            app.note_visible_media(
                "peer@example.invalid".into(),
                vec!["m0".into(), "m1".into()],
                cx,
            );
        });
        cx.executor().run_until_parked();
        assert_eq!(cache.reads.load(Ordering::Relaxed), 1);
        app.read_with(cx, |app, _| {
            assert!(Arc::ptr_eq(
                &app.chats[0].messages[0].media.as_ref().unwrap().data,
                &app.chats[0].messages[1].media.as_ref().unwrap().data,
            ))
        });
    }
    fn open_deferred_viewer(app: &mut WhatsAppApp, cx: &mut Context<WhatsAppApp>) {
        let chat = Arc::make_mut(&mut app.chats[0]);
        for (n, message) in chat.messages.iter_mut().enumerate() {
            let media = message.media.as_mut().unwrap();
            media.data_is_preview = false;
            media.preview_cache_key = None;
            media.cache_key = Some(format!("full-{n}"));
        }
        let viewer = MediaViewer::open(chat.jid.clone(), "m0", &chat.messages).unwrap();
        app.viewer.update(cx, |state, cx| state.open(viewer, cx));
        app.note_media_surface(cx);
    }

    #[gpui::test]
    fn viewer_missing_and_removed_files_end_loading_and_retry_only_after_navigation(
        cx: &mut gpui::TestAppContext,
    ) {
        let (app, cache) = setup(cx);
        cache.fail.store(true, Ordering::Relaxed);
        app.update(cx, |app, cx| {
            open_deferred_viewer(app, cx);
            let message = app.media_viewer_message(cx).unwrap();
            assert!(app.awaiting_cached_media("peer@example.invalid", message));
        });
        cx.executor().run_until_parked();
        app.update(cx, |app, cx| {
            let message = app.media_viewer_message(cx).unwrap();
            assert!(!app.awaiting_cached_media("peer@example.invalid", message));
            assert!(message.media.as_ref().unwrap().data.is_empty());
            assert_eq!(app.viewer.read(cx).showing().unwrap().items.len(), 12);
            for _ in 0..5 {
                app.note_media_surface(cx);
            }
        });
        cx.executor().run_until_parked();
        assert_eq!(
            cache.reads.load(Ordering::Relaxed),
            1,
            "a missing file cannot cause repaint retries"
        );
        cache.fail.store(false, Ordering::Relaxed);
        app.update(cx, |app, cx| {
            app.step_media_viewer(true, cx);
            app.note_media_surface(cx);
        });
        cx.executor().run_until_parked();
        assert_eq!(cache.reads.load(Ordering::Relaxed), 2);
        // Simulate a different cache file being removed while its reference
        // remains in the album. The same path handles native ENOENT.
        cache.fail.store(true, Ordering::Relaxed);
        app.update(cx, |app, cx| {
            app.step_media_viewer(true, cx);
            app.note_media_surface(cx);
        });
        cx.executor().run_until_parked();
        app.read_with(cx, |app, cx| {
            let message = app.media_viewer_message(cx).unwrap();
            assert_eq!(message.id, "m2");
            assert!(!app.awaiting_cached_media("peer@example.invalid", message));
            assert!(message.media.as_ref().unwrap().data.is_empty());
        });
        cache.fail.store(false, Ordering::Relaxed);
        app.update(cx, |app, cx| {
            app.step_media_viewer(false, cx); // ready m1
            app.note_media_surface(cx);
            app.step_media_viewer(false, cx); // retry m0
            app.note_media_surface(cx);
        });
        cx.executor().run_until_parked();
        assert_eq!(cache.reads.load(Ordering::Relaxed), 4);
        app.read_with(cx, |app, cx| {
            assert!(
                !app.media_viewer_message(cx)
                    .unwrap()
                    .media
                    .as_ref()
                    .unwrap()
                    .data
                    .is_empty()
            )
        });
    }

    #[gpui::test]
    fn rapid_viewer_navigation_cancels_old_reads_and_hydrates_only_the_last_image(
        cx: &mut gpui::TestAppContext,
    ) {
        let (app, cache) = setup(cx);
        app.update(cx, |app, cx| {
            open_deferred_viewer(app, cx);
            for _ in 0..2 {
                app.step_media_viewer(true, cx);
                app.note_media_surface(cx);
            }
        });
        cx.executor().run_until_parked();
        assert_eq!(cache.reads.load(Ordering::Relaxed), 1);
        app.read_with(cx, |app, cx| {
            assert_eq!(app.viewer.read(cx).current_id(), Some("m2"));
            for (n, message) in app.chats[0].messages.iter().enumerate() {
                assert_eq!(!message.media.as_ref().unwrap().data.is_empty(), n == 2);
            }
        });
    }

    #[gpui::test]
    fn a_viewer_read_cannot_adopt_into_a_revoked_edited_or_new_account_row(
        cx: &mut gpui::TestAppContext,
    ) {
        let (app, _) = setup(cx);
        for change in 0..4 {
            app.update(cx, |app, cx| {
                Arc::make_mut(&mut app.chats[0]).messages = (0..12).map(message).collect();
                app.media_materialization.surface(None);
                open_deferred_viewer(app, cx);
                match change {
                    0 => Arc::make_mut(&mut app.chats[0]).messages[0].revoked = true,
                    1 => {
                        Arc::make_mut(&mut app.chats[0]).messages[0].content = "new caption".into()
                    }
                    2 => app.document_state_generation += 1,
                    3 => app.account_lid = Some("other-account@example.invalid".into()),
                    _ => unreachable!(),
                }
            });
            cx.executor().run_until_parked();
            app.read_with(cx, |app, _| {
                assert!(
                    app.chats[0].messages[0]
                        .media
                        .as_ref()
                        .unwrap()
                        .data
                        .is_empty()
                )
            });
        }
        // A reference removed by an edit no longer belongs to the album.
        app.update(cx, |app, cx| {
            Arc::make_mut(&mut app.chats[0]).messages[0].media = None;
            app.invalidate_message_cache("peer@example.invalid", cx);
            assert_ne!(app.viewer.read(cx).current_id(), Some("m0"));
        });
    }
    #[cfg(not(target_family = "wasm"))]
    #[gpui::test]
    fn native_retained_history_keeps_a_validated_cache_after_ipc_ends(
        cx: &mut gpui::TestAppContext,
    ) {
        let (app, _) = setup(cx);
        app.update(cx, |app, _| {
            app.materialization_cache = None;
            assert!(app.client.is_none());
            let cache = app
                .media_cache()
                .expect("retained native history has its local cache");
            assert!(cache.defers_message_media());
            assert!(cache.read_deferred("not/a/key", MAX_READ_BYTES).is_err());
            app.chats.clear();
            assert!(
                app.media_cache().is_none(),
                "no disconnected cache is offered without retained history"
            );
        });
    }
    #[gpui::test]
    fn a_failed_full_reference_preserves_inline_bytes_without_claiming_the_full_image_loaded(
        cx: &mut gpui::TestAppContext,
    ) {
        let (app, cache) = setup(cx);
        cache.fail.store(true, Ordering::Relaxed);
        app.update(cx, |app, cx| {
            let chat = Arc::make_mut(&mut app.chats[0]);
            let media = chat.messages[0].media.as_mut().unwrap();
            media.cache_key = Some("missing-full".into());
            media.data = Arc::new(cache.payload.clone());
            let viewer = MediaViewer::open(chat.jid.clone(), "m0", &chat.messages).unwrap();
            app.viewer.update(cx, |state, cx| state.open(viewer, cx));
            app.note_media_surface(cx);
        });
        cx.executor().run_until_parked();
        app.read_with(cx, |app, cx| {
            let message = app.media_viewer_message(cx).unwrap();
            let media = message.media.as_ref().unwrap();
            assert_eq!(media.data.as_slice(), cache.payload.as_slice());
            assert!(media.data_is_preview);
            assert!(!app.awaiting_cached_media("peer@example.invalid", message));
        });
        assert_eq!(cache.reads.load(Ordering::Relaxed), 1);
    }
    #[gpui::test]
    fn dropping_the_owner_cancels_a_read_queued_on_the_background_executor(
        cx: &mut gpui::TestAppContext,
    ) {
        let (app, cache) = setup(cx);
        let weak = app.downgrade();
        app.update(cx, |app, cx| {
            app.visible_chat = Some("peer@example.invalid".into());
            app.note_visible_media("peer@example.invalid".into(), vec!["m0".into()], cx);
        });
        assert_eq!(cache.reads.load(Ordering::Relaxed), 0);
        drop(app);
        cx.update(|_| ()); // flush the entity release before the worker runs
        assert!(weak.upgrade().is_none());
        cx.executor().run_until_parked();
        assert_eq!(
            cache.reads.load(Ordering::Relaxed),
            0,
            "no I/O may start for a destroyed owner"
        );
    }
}
