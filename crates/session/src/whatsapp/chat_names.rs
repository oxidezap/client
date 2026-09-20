//! Display names for special chats, resolved on their own lifecycle.
//!
//! A live inbound message creates its chat row with no name, and history sync
//! only sometimes carries one — so group and channel rows can sit at NULL
//! indefinitely, rendering as "Unnamed group"/"Channel". The name lives on
//! the server's group/channel metadata, and this is the pass that asks for
//! it and writes it back to `chats.name`.
//!
//! It is deliberately not part of history hydration: a name costs a network
//! round trip per chat, and coupling that to a local read would delay every
//! `HistoryLoaded` behind it — the same split the avatar resolver exists
//! for. So the lookup lives here instead, driven by two things: one pass
//! after the session reports `Connected` (which revalidates every special
//! chat, because a subject can change while the process is offline), and one
//! when a live message is first sighted in an unnamed `@g.us`/`@newsletter`
//! (so no restart is needed). A receipt or an ack starts nothing.
//!
//! Only the *name* is resolved here; rendering stays the front end's. What a
//! pass writes goes through the store's writer queue like every other write,
//! and a real change emits `StoreChange::Chats` — which is what re-renders
//! the list, with no extra publish step.
//!
//! The query shape is "selective, not N+1": unnamed groups cost one
//! `get_metadata` each under a small concurrency cap, deduplicated per
//! connection generation; channels cost a single `list_subscribed` that
//! materializes every subscribed name at once, with a selective
//! `get_metadata` only for channels absent from that list. A lookup that
//! fails is deliberately not remembered, so a transient failure is retried
//! the next time its chat is sighted rather than filed as nameless for the
//! life of the session.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use log::{debug, warn};
use oxidezap_chat_store::ChatStore;
use portable_atomic::{AtomicU64, Ordering};
use tokio::sync::{Notify, watch};
use whatsapp_rust::client::Client;
use whatsapp_rust::wacore_binary::jid::{Jid, JidExt};

use super::WhatsAppClient;
use crate::exec::{MaybeSend, spawn_owned};

/// How many per-chat metadata lookups may be in flight at once.
///
/// A per-chat IQ, so the limit is what keeps a first connect on an account of
/// many unnamed groups from opening one IQ per group at once. Four is
/// comfortably inside what the server tolerates while still filling a pass
/// quickly.
const CHAT_NAME_CONCURRENCY: usize = 4;

/// How many stored chats one full pass revalidates.
///
/// The chat list is drawn from a hundred at a time; five pages cover the
/// visible list and the near tail, and the next connect covers any more — the
/// same bound the avatar resolver holds, for the same reason.
const CHAT_NAME_LIST_LIMIT: i64 = 500;

/// Where a special chat's display name is read from.
///
/// A seam, not an abstraction: the session has exactly one implementation and
/// it is the client's own group/newsletter queries. It exists so the pass can
/// be driven deterministically in tests — a live client cannot be asked to
/// fail, delay, or rename on cue, and the answers that matter here are
/// exactly those.
///
/// The bound on the futures is [`MaybeSend`] for the reason every bound in
/// [`crate::exec`] is: the same call is awaited on a work-stealing runtime
/// and on a page's single thread.
///
/// Written as `impl Future` rather than `async fn` on purpose: the sibling
/// [`LidPnSource`](crate::names::LidPnSource) answers an RPITIT the same
/// way, so a caller holding either names it the same way — and the trait
/// stays object-safe-adjacent for the `Arc<T>` blanket below, which an
/// `async fn` in a trait is not.
#[allow(clippy::manual_async_fn)]
pub(crate) trait MetadataSource {
    fn group_subject(&self, jid: &Jid) -> impl Future<Output = Option<String>> + MaybeSend;
    fn subscribed_channels(
        &self,
    ) -> impl Future<Output = Option<HashMap<String, String>>> + MaybeSend;
    fn channel_name(&self, jid: &Jid) -> impl Future<Output = Option<String>> + MaybeSend;
}

impl MetadataSource for Client {
    // `async fn` in a trait is not object-safe-adjacent for the `Arc<T>`
    // blanket below; `impl Future` keeps the call shape while staying
    // callable through it.
    #[allow(clippy::manual_async_fn)]
    fn group_subject(&self, jid: &Jid) -> impl Future<Output = Option<String>> + MaybeSend {
        async move {
            self.groups()
                .get_metadata(jid)
                .await
                .map(|meta| meta.subject)
                .ok()
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn subscribed_channels(
        &self,
    ) -> impl Future<Output = Option<HashMap<String, String>>> + MaybeSend {
        async move {
            self.newsletter()
                .list_subscribed()
                .await
                .map(|subscribed| {
                    subscribed
                        .into_iter()
                        .map(|meta| (meta.jid.to_string(), meta.name))
                        .collect()
                })
                .ok()
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn channel_name(&self, jid: &Jid) -> impl Future<Output = Option<String>> + MaybeSend {
        async move {
            self.newsletter()
                .get_metadata(jid)
                .await
                .map(|meta| meta.name)
                .ok()
        }
    }
}

/// So a caller holding the shared client — which is the resolver task —
/// names it the way it already holds it.
impl<T: MetadataSource + ?Sized> MetadataSource for Arc<T> {
    fn group_subject(&self, jid: &Jid) -> impl Future<Output = Option<String>> + MaybeSend {
        (**self).group_subject(jid)
    }

    fn subscribed_channels(
        &self,
    ) -> impl Future<Output = Option<HashMap<String, String>>> + MaybeSend {
        (**self).subscribed_channels()
    }

    fn channel_name(&self, jid: &Jid) -> impl Future<Output = Option<String>> + MaybeSend {
        (**self).channel_name(jid)
    }
}

/// Asking the chat-name resolver for a pass.
///
/// Explicit state rather than one notification, because the two kinds of ask
/// mean different things and a bare wake-up cannot carry which one it was.
/// A new connection is a new generation: every chat's remembered answer is
/// from the previous socket and no longer counts, so the next full pass
/// revalidates rather than trusting it.
#[derive(Clone, Default)]
pub(super) struct ChatNameResolveSignal {
    ask: Arc<Notify>,
    state: Arc<std::sync::Mutex<PendingNames>>,
    generation: Arc<AtomicU64>,
}

/// What asks are outstanding, under one lock so they are taken atomically.
#[derive(Default)]
struct PendingNames {
    /// Revalidate the stored special chats (used on connect).
    full: bool,
    /// Chats sighted live that may still be unnamed.
    named: HashSet<String>,
}

/// One pass's worth of asks, taken from the signal together.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct NameResolveRequest {
    /// Revalidate the stored special chats.
    pub(super) full: bool,
    /// Chats sighted live that may still be unnamed.
    pub(super) named: Vec<String>,
}

impl ChatNameResolveSignal {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// The connection names are currently resolved under, for the
    /// staleness gate: a pass that started under an older one discards its
    /// answers rather than writing them over newer metadata.
    pub(super) fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// A connection was established: advance the generation counter and ask
    /// for a full pass, because a subject can change while the process is
    /// offline and the previous socket's answers say nothing about this one.
    pub(super) fn new_connection(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.set(|pending| pending.full = true);
    }

    /// Ask about these chats specifically: the first sighting of an
    /// `@g.us`/`@newsletter` that may still be unnamed, so no restart is
    /// needed. Deduplicated by address; an empty ask wakes nobody.
    pub(super) fn request_named(&self, jids: impl IntoIterator<Item = String>) {
        self.set(|pending| {
            let before = pending.named.len();
            pending.named.extend(jids);
            // The guard is inside the lock: two sightings racing must not
            // both conclude they added nothing and both stay quiet, nor both
            // notify — the notify belongs to the state change.
            if pending.named.len() != before {
                self.ask.notify_one();
            }
        });
    }

    fn set(&self, f: impl FnOnce(&mut PendingNames)) {
        let mut pending = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut pending);
        // `request_named` notifies itself when it grows the set; every other
        // mutation here is a kind of ask that must wake the resolver.
        if pending.full {
            self.ask.notify_one();
        }
    }

    /// Wait for an ask, and take what it carried.
    pub(super) async fn next(&self) -> NameResolveRequest {
        loop {
            let taken = {
                let mut pending = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let taken = NameResolveRequest {
                    full: pending.full,
                    named: pending.named.drain().collect(),
                };
                pending.full = false;
                taken
            };
            if taken.full || !taken.named.is_empty() {
                return taken;
            }
            self.ask.notified().await;
        }
    }
}

/// Chats this side has already asked about, by connection generation.
///
/// What makes the resolver safe to nudge on every live message: a chat
/// already asked about costs a hash lookup rather than an IQ, so a busy
/// group does not repeat its traffic per message. A new connection makes
/// every entry stale, because a name can change while the process is
/// offline. A lookup that failed is deliberately not recorded, so a
/// transient failure is retried the next time its chat is sighted.
#[derive(Default)]
pub(super) struct NameResolver {
    asked: HashMap<String, u64>,
}

impl NameResolver {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Whether `jid` still needs a lookup in this generation.
    fn needs(&self, jid: &str, generation: u64) -> bool {
        self.asked.get(jid) != Some(&generation)
    }

    fn mark(&mut self, jid: &str, generation: u64) {
        self.asked.insert(jid.to_string(), generation);
    }
}

/// Whether a stored name is worth keeping.
///
/// One predicate rather than one per caller: the store never persists a blank
/// history name, and the resolver must not write one either — nor treat one
/// already stored as "named" and skip the chat.
fn usable_name(name: &str) -> bool {
    !name.trim().is_empty()
}

/// The stored special chats: every `@g.us` and `@newsletter` row, named or
/// not. A full pass revalidates them all rather than only the unnamed ones,
/// because a subject can change while the process is offline — and a write
/// that learned nothing broadcasts nothing, so an unchanged account costs
/// lookups but no reload.
async fn stored_special_chats(chat_store: &Arc<ChatStore>) -> Vec<Jid> {
    match chat_store.chats(false, CHAT_NAME_LIST_LIMIT).await {
        Ok(entries) => entries
            .into_iter()
            .map(|entry| entry.jid)
            .filter(|jid| jid.is_group() || jid.is_newsletter())
            .collect(),
        Err(e) => {
            debug!("chat-name resolver could not read the chat list: {e}");
            Vec::new()
        }
    }
}

/// Run one pass: decide which chats need a lookup, fetch their names, and
/// write back what was learned.
///
/// The generation is read live at the start and checked again before the
/// write: a pass whose lookups land after a reconnect discards its answers
/// rather than writing the previous socket's metadata over the new one — and
/// marks nothing, so the next pass under the new generation retries.
pub(super) async fn run_pass<S: MetadataSource + ?Sized>(
    source: &S,
    chat_store: &Arc<ChatStore>,
    signal: &ChatNameResolveSignal,
    resolver: &mut NameResolver,
    request: NameResolveRequest,
) {
    let generation = signal.generation();
    let mut groups: Vec<Jid> = Vec::new();
    let mut channels: Vec<Jid> = Vec::new();
    {
        let mut seen: HashSet<String> = HashSet::new();
        let mut push = |jid: Jid| {
            if !seen.insert(jid.to_string()) {
                return;
            }
            if !resolver.needs(&jid.to_string(), generation) {
                return;
            }
            if jid.is_group() {
                groups.push(jid);
            } else if jid.is_newsletter() {
                channels.push(jid);
            }
        };
        for raw in &request.named {
            if let Ok(jid) = raw.parse::<Jid>() {
                push(jid);
            }
        }
        if request.full {
            for jid in stored_special_chats(chat_store).await {
                push(jid);
            }
        }
    }
    if groups.is_empty() && channels.is_empty() {
        return;
    }
    // A sighted chat that is already named needs no lookup: mark it asked so
    // the next message in it costs a hash lookup rather than another store
    // read. A full pass revalidates regardless, which is what picks up a
    // rename that happened while the process was offline.
    if !request.full {
        let mut still_unknown: Vec<Jid> = Vec::with_capacity(groups.len() + channels.len());
        for jid in groups.into_iter().chain(channels) {
            match chat_store.chat(&jid).await {
                Ok(Some(entry)) if entry.name.as_deref().is_some_and(usable_name) => {
                    resolver.mark(&jid.to_string(), generation);
                }
                // No row, or a row with nothing worth keeping: resolve it.
                // A read error is not an answer either, so it resolves too
                // rather than being filed as named for the generation.
                _ => still_unknown.push(jid),
            }
        }
        groups = Vec::new();
        channels = Vec::new();
        for jid in still_unknown {
            if jid.is_group() {
                groups.push(jid);
            } else {
                channels.push(jid);
            }
        }
        if groups.is_empty() && channels.is_empty() {
            return;
        }
    }

    // JIDs whose lookup produced an answer this pass (a usable name or a
    // blank one — both settle the question for this generation). Failures
    // stay unmarked so the next sighting retries them.
    let mut settled: Vec<String> = Vec::new();
    let mut resolved: Vec<(Jid, String)> = Vec::new();
    for chunk in groups.chunks(CHAT_NAME_CONCURRENCY) {
        let answered = whatsapp_rust::futures::future::join_all(chunk.iter().map(|jid| async {
            let name = source.group_subject(jid).await;
            (jid.clone(), name)
        }))
        .await;
        // A reconnect mid-pass ends it here: the remaining chunks — and the
        // one that just landed — belong to the previous socket. Answers are
        // accumulated, never written, until the final gate below confirms
        // the generation is still current, so returning now discards them.
        if signal.generation() != generation {
            debug!("chat-name resolver: discarding a pass from a superseded connection");
            return;
        }
        for (jid, name) in answered {
            // A failed lookup is not remembered: a transient failure
            // retried on the next sighting self-heals, while a memoized
            // one files the chat as nameless until the next reconnect.
            // `if let` rather than `match`, because there is only one
            // answer worth keeping.
            if let Some(name) = name {
                settled.push(jid.to_string());
                if usable_name(&name) {
                    resolved.push((jid, name));
                }
            }
        }
    }
    if !channels.is_empty() {
        // One call for every subscribed channel, which is normally all of
        // them: the per-channel lookup below is only for chats absent from
        // that list.
        let subscribed = source.subscribed_channels().await.unwrap_or_default();
        let mut fallback: Vec<Jid> = Vec::new();
        for jid in channels {
            match subscribed.get(&jid.to_string()) {
                Some(name) => {
                    settled.push(jid.to_string());
                    if usable_name(name) {
                        resolved.push((jid, name.clone()));
                    }
                }
                None => fallback.push(jid),
            }
        }
        for chunk in fallback.chunks(CHAT_NAME_CONCURRENCY) {
            let answered =
                whatsapp_rust::futures::future::join_all(chunk.iter().map(|jid| async {
                    let name = source.channel_name(jid).await;
                    (jid.clone(), name)
                }))
                .await;
            if signal.generation() != generation {
                return;
            }
            for (jid, name) in answered {
                if let Some(name) = name {
                    settled.push(jid.to_string());
                    if usable_name(&name) {
                        resolved.push((jid, name));
                    }
                }
            }
        }
    }

    // The staleness gate: a pass whose lookups landed after a reconnect
    // discards its answers rather than writing the previous socket's
    // metadata over the new one — and marks nothing, so the new
    // generation's pass retries every one of them.
    if signal.generation() != generation {
        debug!("chat-name resolver: discarding a pass from a superseded connection");
        return;
    }
    if !resolved.is_empty() {
        if let Err(e) = chat_store.apply_chat_names(
            resolved
                .iter()
                .map(|(jid, name)| (jid.clone(), name.clone()))
                .collect(),
        ) {
            warn!("chat-name resolver could not queue resolved names: {e}");
            return;
        }
        if let Err(e) = chat_store.flush().await {
            warn!("chat-name resolver's names did not commit: {e}");
            return;
        }
    }
    for jid in settled {
        resolver.mark(&jid, generation);
    }
    debug!(
        "chat-name resolver: {} chat(s) learned a name",
        resolved.len()
    );
}

/// The resolver's whole life: wait for a pass, resolve, repeat.
///
/// One task for the session, like the history reloader and the avatar
/// resolver, and for the same reason: display names are a stream of the
/// account's own metadata and there is no sense in a task per ask. Stops
/// when the session does.
impl WhatsAppClient {
    pub(super) fn spawn_chat_name_resolver(
        client: Arc<Client>,
        chat_store: Arc<ChatStore>,
        signal: ChatNameResolveSignal,
        stopping: watch::Receiver<()>,
    ) {
        spawn_owned(async move {
            let mut resolver = NameResolver::new();
            let mut stopping = stopping;
            loop {
                let request = tokio::select! {
                    request = signal.next() => request,
                    _ = stopping.changed() => return,
                };
                run_pass(&client, &chat_store, &signal, &mut resolver, request).await;
            }
        });
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    /// A metadata source the test drives: names on cue, failures and delays
    /// included — none of which a live client can be asked for.
    struct FakeMeta {
        groups: StdMutex<HashMap<String, String>>,
        channels: StdMutex<HashMap<String, String>>,
        listed: StdMutex<Option<HashMap<String, String>>>,
        fail_groups: portable_atomic::AtomicBool,
    }

    impl FakeMeta {
        fn new() -> Self {
            Self {
                groups: StdMutex::new(HashMap::new()),
                channels: StdMutex::new(HashMap::new()),
                listed: StdMutex::new(Some(HashMap::new())),
                fail_groups: portable_atomic::AtomicBool::new(false),
            }
        }

        fn with_group(name: &str, subject: &str) -> Self {
            let fake = Self::new();
            fake.groups
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(name.to_string(), subject.to_string());
            fake
        }

        fn with_channel(jid: &str, name: &str, listed: bool) -> Self {
            let fake = Self::new();
            fake.channels
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(jid.to_string(), name.to_string());
            if listed {
                fake.listed
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .as_mut()
                    .expect("list is present")
                    .insert(jid.to_string(), name.to_string());
            }
            fake
        }

        fn rename_group(&self, jid: &str, subject: &str) {
            self.groups
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(jid.to_string(), subject.to_string());
        }
    }

    impl MetadataSource for FakeMeta {
        #[allow(clippy::manual_async_fn)]
        fn group_subject(&self, jid: &Jid) -> impl Future<Output = Option<String>> + MaybeSend {
            let answer = (!self.fail_groups.load(Ordering::Relaxed))
                .then(|| {
                    self.groups
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get(&jid.to_string())
                        .cloned()
                })
                .flatten();
            async move { answer }
        }

        #[allow(clippy::manual_async_fn)]
        fn subscribed_channels(
            &self,
        ) -> impl Future<Output = Option<HashMap<String, String>>> + MaybeSend {
            let listed = self
                .listed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            async move { listed }
        }

        #[allow(clippy::manual_async_fn)]
        fn channel_name(&self, jid: &Jid) -> impl Future<Output = Option<String>> + MaybeSend {
            let answer = self
                .channels
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&jid.to_string())
                .cloned();
            async move { answer }
        }
    }

    const GROUP: &str = "120363000000000001@g.us";
    const CHANNEL: &str = "120363400000000001@newsletter";

    async fn test_store(name: &str) -> Arc<ChatStore> {
        let backend = whatsapp_rust::store::SqliteStore::new(&format!(
            "file:oxidezap-names-{name}?mode=memory&cache=shared",
        ))
        .await
        .expect("in-memory store");
        backend.create_new_device().await.expect("seed device");
        ChatStore::new(&backend).await.expect("chat store")
    }

    fn group_message(chat: &str, id: &str) -> whatsapp_rust::wacore::types::events::Event {
        use whatsapp_rust::wacore::proto_helpers::MessageBuilderExt;
        use whatsapp_rust::wacore::types::events::{
            BatchOrigin, Event, InboundMessage, MessageBatch,
        };
        use whatsapp_rust::wacore::types::message::{MessageInfo, MessageSource};
        let info = MessageInfo {
            source: MessageSource {
                chat: chat.parse().expect("test JID"),
                sender: chat.parse().expect("test JID"),
                is_group: chat.ends_with("@g.us"),
                ..Default::default()
            },
            id: id.to_string().into(),
            timestamp: whatsapp_rust::wacore::time::from_secs(1_700_000_000)
                .expect("test timestamp"),
            ..Default::default()
        };
        Event::Messages(
            MessageBatch::builder()
                .messages(Arc::from([InboundMessage::builder()
                    .message(Arc::new(whatsapp_rust::waproto::whatsapp::Message::text(
                        "oi",
                    )))
                    .info(Arc::new(info))
                    .build()]))
                .origin(BatchOrigin::Live)
                .build(),
        )
    }

    async fn feed(store: &Arc<ChatStore>, event: whatsapp_rust::wacore::types::events::Event) {
        store.handler().handle_event(Arc::new(event));
        store.flush().await.expect("flush");
    }

    fn named_request(jids: &[&str]) -> NameResolveRequest {
        NameResolveRequest {
            full: false,
            named: jids.iter().map(|jid| (*jid).to_string()).collect(),
        }
    }

    async fn stored_name(store: &Arc<ChatStore>, jid: &str) -> Option<String> {
        store
            .chat(&jid.parse().expect("test JID"))
            .await
            .expect("read chat")
            .and_then(|entry| entry.name)
    }

    /// A live group message leaves its row nameless; the pass resolves the
    /// subject and persists it.
    #[tokio::test]
    async fn a_live_group_row_resolves_to_its_subject() {
        let store = test_store("group-subject").await;
        feed(&store, group_message(GROUP, "MSG-G1")).await;
        assert_eq!(stored_name(&store, GROUP).await, None);

        let source = FakeMeta::with_group(GROUP, "Trip planning");
        let signal = ChatNameResolveSignal::new();
        let mut resolver = NameResolver::new();
        run_pass(
            &source,
            &store,
            &signal,
            &mut resolver,
            named_request(&[GROUP]),
        )
        .await;

        assert_eq!(
            stored_name(&store, GROUP).await.as_deref(),
            Some("Trip planning")
        );
    }

    /// A live channel message resolves through the subscribed list, with no
    /// per-channel lookup spent on a chat the list already names.
    #[tokio::test]
    async fn a_live_channel_row_resolves_to_its_name() {
        let store = test_store("channel-name").await;
        feed(&store, group_message(CHANNEL, "MSG-C1")).await;
        assert_eq!(stored_name(&store, CHANNEL).await, None);

        let source = FakeMeta::with_channel(CHANNEL, "Announcements", true);
        let signal = ChatNameResolveSignal::new();
        let mut resolver = NameResolver::new();
        run_pass(
            &source,
            &store,
            &signal,
            &mut resolver,
            named_request(&[CHANNEL]),
        )
        .await;

        assert_eq!(
            stored_name(&store, CHANNEL).await.as_deref(),
            Some("Announcements")
        );
    }

    /// A channel absent from the subscribed list falls back to a selective
    /// metadata lookup rather than staying nameless.
    #[tokio::test]
    async fn a_channel_absent_from_the_list_falls_back_to_metadata() {
        let store = test_store("channel-fallback").await;
        feed(&store, group_message(CHANNEL, "MSG-C2")).await;

        let source = FakeMeta::with_channel(CHANNEL, "Quiet updates", false);
        let signal = ChatNameResolveSignal::new();
        let mut resolver = NameResolver::new();
        run_pass(
            &source,
            &store,
            &signal,
            &mut resolver,
            named_request(&[CHANNEL]),
        )
        .await;

        assert_eq!(
            stored_name(&store, CHANNEL).await.as_deref(),
            Some("Quiet updates")
        );
    }

    /// A metadata failure leaves the fallback rendering alone: no row, no
    /// broadcast-worthy write, and no memory of the failure — the next
    /// sighting retries rather than filing the chat as nameless.
    #[tokio::test]
    async fn a_metadata_failure_keeps_the_fallback() {
        let store = test_store("group-failure").await;
        feed(&store, group_message(GROUP, "MSG-G2")).await;

        let source = FakeMeta::with_group(GROUP, "Trip planning");
        source.fail_groups.store(true, Ordering::Relaxed);
        let signal = ChatNameResolveSignal::new();
        let mut resolver = NameResolver::new();
        run_pass(
            &source,
            &store,
            &signal,
            &mut resolver,
            named_request(&[GROUP]),
        )
        .await;
        assert_eq!(stored_name(&store, GROUP).await, None);

        // And the failure bought no dedup entry: healing the source and
        // asking again resolves, without a reconnect in between.
        source.fail_groups.store(false, Ordering::Relaxed);
        run_pass(
            &source,
            &store,
            &signal,
            &mut resolver,
            named_request(&[GROUP]),
        )
        .await;
        assert_eq!(
            stored_name(&store, GROUP).await.as_deref(),
            Some("Trip planning")
        );
    }

    /// A reconnect revalidates: the subject renamed while the process was
    /// offline is picked up by the next full pass, not kept at its stale
    /// answer.
    #[tokio::test]
    async fn a_reconnect_updates_a_rename() {
        let store = test_store("group-rename").await;
        feed(&store, group_message(GROUP, "MSG-G3")).await;

        let source = FakeMeta::with_group(GROUP, "Trip planning");
        let signal = ChatNameResolveSignal::new();
        let mut resolver = NameResolver::new();
        run_pass(
            &source,
            &store,
            &signal,
            &mut resolver,
            named_request(&[GROUP]),
        )
        .await;
        assert_eq!(
            stored_name(&store, GROUP).await.as_deref(),
            Some("Trip planning")
        );

        source.rename_group(GROUP, "Trip planning v2");
        signal.new_connection();
        // The generation its asks were taken under is spent: take the new
        // one the reconnect queued.
        let request = signal.next().await;
        assert!(request.full);
        run_pass(&source, &store, &signal, &mut resolver, request).await;
        assert_eq!(
            stored_name(&store, GROUP).await.as_deref(),
            Some("Trip planning v2")
        );
    }

    /// A pass whose lookups land after a reconnect discards its answers
    /// rather than writing the previous socket's metadata over the new one
    /// — and marks nothing, so the new generation retries.
    ///
    /// Deterministic rather than timing-dependent: the lookup holds itself
    /// open until the reconnect has landed, so the pass is provably
    /// mid-flight under the old generation when the bump lands.
    #[tokio::test]
    async fn a_stale_generation_late_response_writes_nothing() {
        // The late-response half, through a racing pass: the pass enters
        // under generation G, a reconnect bumps the signal to G+1 while its
        // one lookup is still in flight, and the gate discards the answer
        // rather than writing the previous socket's metadata over the new
        // one — marking nothing, so the new generation retries.
        //
        // The interleaving is ordered, not timed: the lookup signals entry
        // and then holds itself open until released, so the bump provably
        // lands mid-flight. The pass runs on a spawned task because awaiting
        // it here would serialize the bump behind it; its outcome travels
        // back over a channel because the resolver it borrows cannot cross
        // the spawn — so the spawned task owns a fresh one, and the
        // discarded pass's "marked nothing" half is asserted on the outer
        // resolver below instead.
        struct Gated {
            entered: Arc<tokio::sync::Notify>,
            release: Arc<tokio::sync::Notify>,
        }
        struct SlowGate<'a> {
            inner: &'a FakeMeta,
            gate: Arc<Gated>,
        }
        impl MetadataSource for SlowGate<'_> {
            #[allow(clippy::manual_async_fn)]
            fn group_subject(&self, jid: &Jid) -> impl Future<Output = Option<String>> + MaybeSend {
                async move {
                    self.gate.entered.notify_one();
                    self.gate.release.notified().await;
                    self.inner.group_subject(jid).await
                }
            }
            #[allow(clippy::manual_async_fn)]
            fn subscribed_channels(
                &self,
            ) -> impl Future<Output = Option<HashMap<String, String>>> + MaybeSend {
                async move { Some(HashMap::new()) }
            }
            #[allow(clippy::manual_async_fn)]
            fn channel_name(&self, jid: &Jid) -> impl Future<Output = Option<String>> + MaybeSend {
                let jid = jid.clone();
                async move { self.inner.channel_name(&jid).await }
            }
        }
        let store = test_store("group-stale").await;
        feed(&store, group_message(GROUP, "MSG-G4")).await;

        let source = std::sync::Arc::new(FakeMeta::with_group(GROUP, "Stale answer"));
        let signal = ChatNameResolveSignal::new();
        let opened = signal.generation();
        let mut resolver = NameResolver::new();

        let gate = Arc::new(Gated {
            entered: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
        });
        // The bump waits for the lookup to signal entry, so it provably
        // lands mid-flight rather than before the pass starts.
        let bump_signal = signal.clone();
        let bump_gate = gate.clone();
        let bump = tokio::spawn(async move {
            bump_gate.entered.notified().await;
            bump_signal.new_connection();
        });
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let pass_store = store.clone();
        let pass_signal = signal.clone();
        let pass_gate = gate.clone();
        let pass_source = source.clone();
        tokio::spawn(async move {
            let slow = SlowGate {
                inner: &pass_source,
                gate: pass_gate,
            };
            let mut pass_resolver = NameResolver::new();
            run_pass(
                &slow,
                &pass_store,
                &pass_signal,
                &mut pass_resolver,
                named_request(&[GROUP]),
            )
            .await;
            let _ = done_tx.send(());
        });
        // The entry signal fires from inside the lookup, so by the time the
        // bump is awaited here the pass is provably inside its network wait
        // — not still queued behind the executor.
        bump.await.expect("reconnect lands mid-pass");
        assert_ne!(
            signal.generation(),
            opened,
            "the reconnect advanced the generation"
        );
        gate.release.notify_one();
        tokio::time::timeout(Duration::from_secs(10), done_rx)
            .await
            .expect("the gated pass finishes")
            .expect("the pass sends its outcome");

        assert_eq!(
            stored_name(&store, GROUP).await,
            None,
            "the previous socket's answer must not overwrite the new generation"
        );

        // The reconnect queued its own full ask: drained under the new
        // generation, the same answer now writes, because it is current.
        // And the discarded pass marked nothing, so the retry is not
        // skipped as already-asked.
        assert!(
            resolver.needs(GROUP, signal.generation()),
            "the discarded pass must not mark the chat asked"
        );
        let mut drained = false;
        while let Ok(request) = tokio::time::timeout(Duration::from_secs(5), signal.next()).await {
            drained = drained || request.full;
            run_pass(&source, &store, &signal, &mut resolver, request).await;
            if stored_name(&store, GROUP).await.is_some() {
                break;
            }
        }
        assert!(drained, "the reconnect queued its own full pass");
        assert_eq!(
            stored_name(&store, GROUP).await.as_deref(),
            Some("Stale answer")
        );
    }

    /// The dedup is what keeps a busy group from repeating its traffic: a
    /// chat asked about costs a hash lookup rather than an IQ.
    #[test]
    fn an_asked_chat_is_not_asked_again() {
        let mut resolver = NameResolver::new();
        assert!(resolver.needs(GROUP, 1));
        resolver.mark(GROUP, 1);
        assert!(!resolver.needs(GROUP, 1));
        assert!(resolver.needs(GROUP, 2), "a new connection revalidates");
    }

    /// A blank subject settles nothing to write, but settles the question:
    /// the pass must not loop a lookup per message on a chat whose metadata
    /// has no usable name.
    #[tokio::test]
    async fn a_blank_subject_writes_nothing_but_settles() {
        let store = test_store("group-blank").await;
        feed(&store, group_message(GROUP, "MSG-G5")).await;

        let source = FakeMeta::with_group(GROUP, "   ");
        let signal = ChatNameResolveSignal::new();
        let mut resolver = NameResolver::new();
        run_pass(
            &source,
            &store,
            &signal,
            &mut resolver,
            named_request(&[GROUP]),
        )
        .await;

        assert_eq!(stored_name(&store, GROUP).await, None);
        assert!(
            !resolver.needs(GROUP, signal.generation()),
            "a blank answer still settles the generation"
        );
    }
}
