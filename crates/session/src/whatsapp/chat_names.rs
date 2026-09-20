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
use crate::exec::{MaybeSend, sleep, spawn_owned};

/// How many per-chat metadata lookups may be in flight at once.
///
/// A per-chat IQ, so the limit is what keeps a first connect on an account of
/// many unnamed groups from opening one IQ per group at once. Four is
/// comfortably inside what the server tolerates while still filling a pass
/// quickly.
const CHAT_NAME_CONCURRENCY: usize = 4;

/// Whether a failed request only affects its chat or indicates account-wide
/// throttling. Only a server-provided IQ backoff is global; ordinary lookup
/// failures must not stop unrelated chats from being resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryScope {
    /// Cool only the JID that failed.
    Chat,
    /// Stop work not started yet and retry the pass after the delay.
    Global,
}

/// Retry information returned by a metadata operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NameRetry {
    pub(crate) retry_after: std::time::Duration,
    pub(crate) scope: RetryScope,
}

/// What one metadata lookup told us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NameLookup {
    /// The server named the chat (possibly blank, which settles the
    /// question for this generation without writing anything).
    Found(String),
    /// The request failed. A global failure is a server-directed throttle;
    /// other failures cool only this chat.
    Failed {
        retry_after: std::time::Duration,
        scope: RetryScope,
    },
}

/// Fallback cooldown when the server named no delay: transport hiccups and
/// timeouts retry soon, but never on the very next message.
const NAME_RETRY_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);

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
    fn group_subject(&self, jid: &Jid) -> impl Future<Output = NameLookup> + MaybeSend;
    fn subscribed_channels(
        &self,
    ) -> impl Future<Output = Result<HashMap<String, String>, NameRetry>> + MaybeSend;
    fn channel_name(&self, jid: &Jid) -> impl Future<Output = NameLookup> + MaybeSend;
}

impl MetadataSource for Client {
    // `async fn` in a trait is not object-safe-adjacent for the `Arc<T>`
    // blanket below; `impl Future` keeps the call shape while staying
    // callable through it.
    #[allow(clippy::manual_async_fn)]
    fn group_subject(&self, jid: &Jid) -> impl Future<Output = NameLookup> + MaybeSend {
        async move {
            match self.groups().get_metadata(jid).await {
                Ok(meta) => NameLookup::Found(meta.subject),
                Err(e) => {
                    let retry = name_retry_after(&e);
                    NameLookup::Failed {
                        retry_after: retry.retry_after,
                        scope: retry.scope,
                    }
                }
            }
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn subscribed_channels(
        &self,
    ) -> impl Future<Output = Result<HashMap<String, String>, NameRetry>> + MaybeSend {
        async move {
            match self.newsletter().list_subscribed().await {
                Ok(subscribed) => Ok(subscribed
                    .into_iter()
                    .map(|meta| (meta.jid.to_string(), meta.name))
                    .collect()),
                Err(e) => Err(name_retry_after(&e)),
            }
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn channel_name(&self, jid: &Jid) -> impl Future<Output = NameLookup> + MaybeSend {
        async move {
            match self.newsletter().get_metadata(jid).await {
                Ok(meta) => NameLookup::Found(meta.name),
                Err(e) => {
                    let retry = name_retry_after(&e);
                    NameLookup::Failed {
                        retry_after: retry.retry_after,
                        scope: retry.scope,
                    }
                }
            }
        }
    }
}

/// Classify a failed lookup without turning an ordinary error into a global
/// throttle. The client is pinned on a protocol where only
/// `ServerError.backoff` is evidence that the account must stop issuing more
/// metadata requests; 403/404, timeouts and local failures cool one JID.
fn name_retry_after(error: &impl NameErrorBackoff) -> NameRetry {
    match error.backoff_secs() {
        Some(secs) => NameRetry {
            retry_after: std::time::Duration::from_secs(u64::from(secs.max(1))),
            scope: RetryScope::Global,
        },
        None => NameRetry {
            retry_after: NAME_RETRY_COOLDOWN,
            scope: RetryScope::Chat,
        },
    }
}

/// The one field of a metadata failure the resolver acts on.
///
/// A tiny trait rather than a match per call site: group and newsletter
/// failures are different types with the same question, and the question
/// is asked in exactly one place above per lookup kind.
trait NameErrorBackoff {
    fn backoff_secs(&self) -> Option<u32>;
}

impl NameErrorBackoff for whatsapp_rust::features::GroupError {
    fn backoff_secs(&self) -> Option<u32> {
        use whatsapp_rust::features::GroupError;
        use whatsapp_rust::request::IqError;
        match self {
            GroupError::Iq(IqError::ServerError { backoff, .. }) => *backoff,
            _ => None,
        }
    }
}

impl NameErrorBackoff for whatsapp_rust::features::NewsletterError {
    fn backoff_secs(&self) -> Option<u32> {
        use whatsapp_rust::features::{MexError, NewsletterError};
        use whatsapp_rust::request::IqError;
        match self {
            NewsletterError::Mex(MexError::Request(IqError::ServerError { backoff, .. }))
            | NewsletterError::Iq(IqError::ServerError { backoff, .. }) => *backoff,
            _ => None,
        }
    }
}

/// So a caller holding the shared client — which is the resolver task —
/// names it the way it already holds it.
impl<T: MetadataSource + ?Sized> MetadataSource for Arc<T> {
    fn group_subject(&self, jid: &Jid) -> impl Future<Output = NameLookup> + MaybeSend {
        (**self).group_subject(jid)
    }

    fn subscribed_channels(
        &self,
    ) -> impl Future<Output = Result<HashMap<String, String>, NameRetry>> + MaybeSend {
        (**self).subscribed_channels()
    }

    fn channel_name(&self, jid: &Jid) -> impl Future<Output = NameLookup> + MaybeSend {
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
        self.request_full();
    }

    /// Ask for a full pass over the stored special chats, under the current
    /// generation. For post-sync repair (`OfflineSyncCompleted`): whatever
    /// the drain materialized after the connect snapshot is in the store
    /// now, so re-cover it without advancing the generation — the answers
    /// still belong to this socket.
    pub(super) fn request_full(&self) {
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
/// offline.
///
/// Three states per chat, because "has a name" and "was revalidated"
/// are different claims. A full pass that FAILS a lookup leaves the chat
/// unmarked, so a later sighting retries rather than accepting the stale
/// stored name as sufficient (a non-full pass skips only chats the CURRENT
/// generation settled). A failed lookup still records a cooldown, so a
/// busy unnamed group retries on the next sighting after the delay — never
/// on every message, and never before the server's own backoff.
#[derive(Default)]
pub(super) struct NameResolver {
    asked: HashMap<String, u64>,
    cooling: HashMap<String, (u64, wacore::time::Instant)>,
    global_cooling: Option<(u64, wacore::time::Instant)>,
}

impl NameResolver {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Whether `jid` still needs a lookup in this generation: unasked, or
    /// asked-and-failed with its cooldown elapsed.
    fn needs(&self, jid: &str, generation: u64) -> bool {
        if self.asked.get(jid) == Some(&generation) {
            return false;
        }
        if let Some((global_generation, until)) = self.global_cooling
            && global_generation == generation
            && until.elapsed().as_nanos() == 0
        {
            return false;
        }
        match self.cooling.get(jid) {
            Some((gen_number, until)) if *gen_number == generation => {
                until.elapsed().as_nanos() > 0
            }
            _ => true,
        }
    }

    fn mark(&mut self, jid: &str, generation: u64) {
        self.asked.insert(jid.to_string(), generation);
        self.cooling.remove(jid);
    }

    /// A failed lookup: retryable after `retry_after`, not on the next
    /// message. Deliberately NOT marked asked — the chat stays due, and a
    /// later sighting past the cooldown looks it up again.
    fn cool(&mut self, jid: &str, generation: u64, retry_after: std::time::Duration) {
        self.cooling.insert(
            jid.to_string(),
            (generation, wacore::time::Instant::now() + retry_after),
        );
    }

    fn cool_global(&mut self, generation: u64, retry_after: std::time::Duration) {
        let until = wacore::time::Instant::now() + retry_after;
        let replace = match self.global_cooling {
            None => true,
            Some((current_generation, current_until)) => {
                current_generation != generation || current_until < until
            }
        };
        if replace {
            self.global_cooling = Some((generation, until));
        }
    }

    /// Remaining delay before a global server backoff may be retried.
    fn global_retry_after(&self, generation: u64) -> Option<std::time::Duration> {
        let (cooling_generation, until) = self.global_cooling?;
        if cooling_generation != generation {
            return None;
        }
        let remaining = until.saturating_duration_since(wacore::time::Instant::now());
        (!remaining.is_zero()).then_some(remaining)
    }

    /// Advance a chat past its cooldown the way its expiry would, without
    /// sleeping out the wall clock. Test-only: production waits.
    #[cfg(test)]
    fn expire_cooldown(&mut self, jid: &str) {
        self.cooling.remove(jid);
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
/// not — archived included, since the archived list draws them with the
/// same fallback. A full pass revalidates them all rather than only the
/// unnamed ones, because a subject can change while the process is offline
/// — and a write that learned nothing broadcasts nothing, so an unchanged
/// account costs lookups but no reload.
///
/// Read from the dedicated query rather than the paged chat list: names are
/// durable per-chat data, not viewport data like avatar bytes, so a page
/// bound (and its pinned-first order) has no business deciding which chats
/// get revalidated.
async fn stored_special_chats(chat_store: &Arc<ChatStore>) -> Vec<(Jid, Option<String>)> {
    match chat_store.special_chat_names().await {
        Ok(rows) => rows,
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
    stop: &mut watch::Receiver<()>,
) {
    let generation = signal.generation();
    // The event handler and the ChatStore have independent subscriptions. A
    // completion/sighting can therefore wake this task while the materializer
    // still has history or the message that caused the sighting in its queue.
    // Make the snapshot a real barrier before consuming the ask.
    if request.full || !request.named.is_empty() {
        let flushed = chat_store.flush();
        tokio::pin!(flushed);
        let flushed = tokio::select! {
            result = &mut flushed => result,
            _ = stop.changed() => return,
        };
        if let Err(e) = flushed {
            warn!("chat-name resolver could not flush the chat store: {e}");
            return;
        }
    }
    // What the rows held when this pass started, keyed by address. The
    // CAS writes below compare against THESE values — not a re-read at
    // write time — so a live rename that commits mid-flight is never
    // clobbered by the older answer. Read once, up front, in one query;
    // a chat created after this snapshot is simply not in this pass
    // (its sighting queues its own ask).
    let mut pre: HashMap<String, Option<String>> = HashMap::new();
    let mut groups: Vec<Jid> = Vec::new();
    let mut channels: Vec<Jid> = Vec::new();
    {
        let mut seen: HashSet<String> = HashSet::new();
        // `push` borrows `seen`/`resolver`/`pre`/the vecs; the sighting
        // loop below needs `seen` back for its contains-check, so it is
        // scoped to the full-pass fill and the sightings push inline.
        {
            let mut push = |jid: Jid, stored: Option<String>| {
                if !seen.insert(jid.to_string()) {
                    return;
                }
                if !resolver.needs(&jid.to_string(), generation) {
                    return;
                }
                pre.insert(jid.to_string(), stored);
                if jid.is_group() {
                    groups.push(jid);
                } else if jid.is_newsletter() {
                    channels.push(jid);
                }
            };
            if request.full {
                for (jid, stored) in stored_special_chats(chat_store).await {
                    push(jid, stored);
                }
            }
        }
        // Sightings too: a chat created after the snapshot (offline drain
        // still materializing, a history chunk landing late) carries its
        // own ask rather than waiting for the next reconnect. Its stored
        // value is read per chat — sightings are a handful, not a page —
        // and a read error resolves rather than being filed as named.
        for raw in &request.named {
            let Ok(jid) = raw.parse::<Jid>() else {
                continue;
            };
            if seen.contains(&jid.to_string()) {
                continue;
            }
            let stored = match chat_store.chat(&jid).await {
                Ok(Some(entry)) => entry.name,
                Ok(None) => None,
                Err(e) => {
                    debug!(
                        "chat-name resolver could not read {}: {e}",
                        jid.to_non_ad_string()
                    );
                    None
                }
            };
            // A non-full sighting of an already-named chat needs no
            // lookup — UNLESS this generation owes it a retry: a full
            // pass that failed it left it unmarked precisely so a later
            // sighting would try the network again, and accepting the
            // stale stored name here would lose the offline rename until
            // the next reconnect. `needs` already encodes that: asked
            // chats skip, failed-and-cooled chats retry.
            if !request.full
                && stored.as_deref().is_some_and(usable_name)
                && !resolver.needs(&jid.to_string(), generation)
            {
                continue;
            }
            if !seen.insert(jid.to_string()) {
                continue;
            }
            if !resolver.needs(&jid.to_string(), generation) {
                continue;
            }
            pre.insert(jid.to_string(), stored);
            if jid.is_group() {
                groups.push(jid);
            } else if jid.is_newsletter() {
                channels.push(jid);
            }
        }
    }
    if groups.is_empty() && channels.is_empty() {
        return;
    }
    // `stop` races the network below, not just the wait for the next
    // ask: on teardown (notably a page's, where `spawn_owned` tasks are
    // accounted and waited on) a pass stuck in a hung metadata request
    // must not hold the close grace hostage. Each race is written out
    // below: `stop.changed()` borrows `stop` for the select's lifetime,
    // so the awaits cannot share one helper.

    // JIDs whose lookup produced an answer this pass (a usable name or a
    // blank one — both settle the question for this generation). Failures
    // record a cooldown (server backoff honored) so the next sighting
    // retries after the delay rather than on the next message.
    let mut settled: Vec<String> = Vec::new();
    let mut resolved: Vec<oxidezap_chat_store::ChatNameWrite> = Vec::new();
    let mut failed: Vec<(String, std::time::Duration)> = Vec::new();
    let mut global_backoff: Option<std::time::Duration> = None;
    for (chunk_index, chunk) in groups.chunks(CHAT_NAME_CONCURRENCY).enumerate() {
        let lookup = whatsapp_rust::futures::future::join_all(chunk.iter().map(|jid| async {
            let name = source.group_subject(jid).await;
            (jid.clone(), name)
        }));
        tokio::pin!(lookup);
        let answered = tokio::select! {
            out = &mut lookup => out,
            _ = stop.changed() => return,
        };
        // A reconnect mid-pass ends it here: the remaining chunks — and the
        // one that just landed — belong to the previous socket. Answers are
        // accumulated, never written, until the final gate below confirms
        // the generation is still current, so returning now discards them.
        if signal.generation() != generation {
            debug!("chat-name resolver: discarding a pass from a superseded connection");
            return;
        }
        for (jid, name) in answered {
            match name {
                NameLookup::Found(name) => {
                    let key = jid.to_string();
                    settled.push(key.clone());
                    if usable_name(&name) {
                        resolved.push(oxidezap_chat_store::ChatNameWrite::checked(
                            jid,
                            pre.get(&key).cloned().flatten(),
                            name,
                        ));
                    }
                }
                // Failed, not forgotten-without-a-trace: the cooldown is
                // what keeps a busy unnamed group from re-requesting on
                // every message while still retrying on a later sighting.
                NameLookup::Failed {
                    retry_after,
                    scope: RetryScope::Chat,
                } => {
                    failed.push((jid.to_string(), retry_after));
                }
                NameLookup::Failed {
                    retry_after,
                    scope: RetryScope::Global,
                } => {
                    global_backoff = Some(
                        global_backoff.map_or(retry_after, |current| current.max(retry_after)),
                    );
                    failed.push((jid.to_string(), retry_after));
                }
            }
        }
        if let Some(retry_after) = global_backoff {
            // A server-directed backoff applies to work not started too. Do
            // not immediately issue every later chunk into the same throttle
            // window; those JIDs will be retried by the timer-driven pass.
            let remaining_start = (chunk_index + 1) * CHAT_NAME_CONCURRENCY;
            failed.extend(
                groups
                    .iter()
                    .skip(remaining_start)
                    .map(|jid| (jid.to_string(), retry_after)),
            );
            break;
        }
    }
    if !channels.is_empty() && global_backoff.is_none() {
        // One call for every subscribed channel, which is normally all of
        // them — but ONLY when the bulk call succeeds. A failed list must
        // not read as "every channel absent": that turns one transient
        // failure of the cheapest call into per-channel requests for all
        // of them, exactly while the service is down. On failure the whole
        // channel half stays pending (cooled per chat) for a later pass.
        let listed = source.subscribed_channels();
        tokio::pin!(listed);
        let listed = tokio::select! {
            out = &mut listed => out,
            _ = stop.changed() => return,
        };
        match listed {
            Ok(subscribed) => {
                let mut fallback: Vec<Jid> = Vec::new();
                for jid in channels {
                    match subscribed.get(&jid.to_string()) {
                        Some(name) => {
                            let key = jid.to_string();
                            settled.push(key.clone());
                            if usable_name(name) {
                                resolved.push(oxidezap_chat_store::ChatNameWrite::checked(
                                    jid,
                                    pre.get(&key).cloned().flatten(),
                                    name.clone(),
                                ));
                            }
                        }
                        None => fallback.push(jid),
                    }
                }
                // A channel absent from a GOOD list is either unsubscribed
                // or too new for it. A full pass avoids the N+1 fallback for
                // ordinary stored rows, while a live sighting is stronger
                // evidence and earns its selective lookup. A chat that
                // already carries a usable stored name still retries when a
                // previous lookup failed and its cooldown has elapsed.
                let explicitly_sighted: HashSet<String> = request
                    .named
                    .iter()
                    .filter_map(|raw| raw.parse::<Jid>().ok())
                    .map(|jid| jid.to_string())
                    .collect();
                let fallback: Vec<Jid> = fallback
                    .into_iter()
                    .filter(|jid| {
                        let key = jid.to_string();
                        // A full pass normally avoids fanning out for rows
                        // absent from the subscribed list. A sighting is
                        // stronger evidence: an unsubscribed/new channel can
                        // only be resolved through this fallback.
                        let eligible = !request.full || explicitly_sighted.contains(&key);
                        let known = pre
                            .get(&key)
                            .and_then(|stored| stored.as_deref())
                            .is_some_and(usable_name);
                        eligible
                            && resolver.needs(&key, generation)
                            && (!known || explicitly_sighted.contains(&key))
                    })
                    .collect();
                for (chunk_index, chunk) in fallback.chunks(CHAT_NAME_CONCURRENCY).enumerate() {
                    let lookup =
                        whatsapp_rust::futures::future::join_all(chunk.iter().map(|jid| async {
                            let name = source.channel_name(jid).await;
                            (jid.clone(), name)
                        }));
                    tokio::pin!(lookup);
                    let answered = tokio::select! {
                        out = &mut lookup => out,
                        _ = stop.changed() => return,
                    };
                    if signal.generation() != generation {
                        return;
                    }
                    for (jid, name) in answered {
                        match name {
                            NameLookup::Found(name) => {
                                let key = jid.to_string();
                                settled.push(key.clone());
                                if usable_name(&name) {
                                    resolved.push(oxidezap_chat_store::ChatNameWrite::checked(
                                        jid,
                                        pre.get(&key).cloned().flatten(),
                                        name,
                                    ));
                                }
                            }
                            NameLookup::Failed {
                                retry_after,
                                scope: RetryScope::Chat,
                            } => {
                                failed.push((jid.to_string(), retry_after));
                            }
                            NameLookup::Failed {
                                retry_after,
                                scope: RetryScope::Global,
                            } => {
                                global_backoff = Some(
                                    global_backoff
                                        .map_or(retry_after, |current| current.max(retry_after)),
                                );
                                failed.push((jid.to_string(), retry_after));
                            }
                        }
                    }
                    if let Some(retry_after) = global_backoff {
                        let remaining_start = (chunk_index + 1) * CHAT_NAME_CONCURRENCY;
                        failed.extend(
                            fallback
                                .iter()
                                .skip(remaining_start)
                                .map(|jid| (jid.to_string(), retry_after)),
                        );
                        break;
                    }
                }
            }
            Err(retry) => {
                // The list itself failed: cool every channel of this pass
                // rather than treating them as absent. Nothing is settled,
                // nothing fans out, and a later sighting past the cooldown
                // retries the bulk call first.
                if retry.scope == RetryScope::Global {
                    global_backoff = Some(
                        global_backoff
                            .map_or(retry.retry_after, |current| current.max(retry.retry_after)),
                    );
                }
                for jid in channels {
                    failed.push((jid.to_string(), retry.retry_after));
                }
            }
        }
    } else if let Some(retry_after) = global_backoff {
        // A group request was rate-limited. Do not spend the same pass on a
        // channel bulk request either; cool its pending rows with the same
        // server-directed delay and let the next pass start cleanly.
        failed.extend(channels.iter().map(|jid| (jid.to_string(), retry_after)));
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
        if let Err(e) = chat_store.apply_chat_names(resolved.clone()) {
            warn!("chat-name resolver could not queue resolved names: {e}");
            return;
        }
        let flushed = chat_store.flush();
        tokio::pin!(flushed);
        let flushed = tokio::select! {
            out = &mut flushed => out,
            _ = stop.changed() => return,
        };
        if let Err(e) = flushed {
            warn!("chat-name resolver's names did not commit: {e}");
            return;
        }
    }
    for jid in settled {
        resolver.mark(&jid, generation);
    }
    for (jid, retry_after) in failed {
        resolver.cool(&jid, generation, retry_after);
    }
    if let Some(retry_after) = global_backoff {
        resolver.cool_global(generation, retry_after);
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
                // A global server backoff cancels work that was not started,
                // so it must also schedule the pass that retries that work.
                // One timer for the resolver is enough; per-chat failures
                // remain event-driven and never turn into timer storms.
                let request = if let Some(delay) = resolver.global_retry_after(signal.generation())
                {
                    tokio::select! {
                        request = signal.next() => request,
                        _ = sleep(delay) => {
                            signal.request_full();
                            continue;
                        },
                        _ = stopping.changed() => return,
                    }
                } else {
                    tokio::select! {
                        request = signal.next() => request,
                        _ = stopping.changed() => return,
                    }
                };
                // The pass itself races teardown too: `stopping` is threaded
                // through so a shutdown mid-lookup ends the pass instead of
                // holding the close grace behind hundreds of network calls.
                run_pass(
                    &client,
                    &chat_store,
                    &signal,
                    &mut resolver,
                    request,
                    &mut stopping,
                )
                .await;
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
        fail_list: portable_atomic::AtomicBool,
    }

    impl FakeMeta {
        fn new() -> Self {
            Self {
                groups: StdMutex::new(HashMap::new()),
                channels: StdMutex::new(HashMap::new()),
                listed: StdMutex::new(Some(HashMap::new())),
                fail_groups: portable_atomic::AtomicBool::new(false),
                fail_list: portable_atomic::AtomicBool::new(false),
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
        fn group_subject(&self, jid: &Jid) -> impl Future<Output = NameLookup> + MaybeSend {
            let answer = if self.fail_groups.load(Ordering::Relaxed) {
                NameLookup::Failed {
                    retry_after: NAME_RETRY_COOLDOWN,
                    scope: RetryScope::Chat,
                }
            } else {
                match self
                    .groups
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&jid.to_string())
                    .cloned()
                {
                    Some(name) => NameLookup::Found(name),
                    // Unknown to the fake = network failure, not a blank
                    // subject: a blank settles, a failure cools. Tests that
                    // want a blank answer insert one explicitly.
                    None => NameLookup::Failed {
                        retry_after: NAME_RETRY_COOLDOWN,
                        scope: RetryScope::Chat,
                    },
                }
            };
            async move { answer }
        }

        #[allow(clippy::manual_async_fn)]
        fn subscribed_channels(
            &self,
        ) -> impl Future<Output = Result<HashMap<String, String>, NameRetry>> + MaybeSend {
            let listed = if self.fail_list.load(Ordering::Relaxed) {
                Err(NameRetry {
                    retry_after: NAME_RETRY_COOLDOWN,
                    scope: RetryScope::Chat,
                })
            } else {
                self.listed
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
                    .ok_or(NameRetry {
                        retry_after: NAME_RETRY_COOLDOWN,
                        scope: RetryScope::Chat,
                    })
            };
            async move { listed }
        }

        #[allow(clippy::manual_async_fn)]
        fn channel_name(&self, jid: &Jid) -> impl Future<Output = NameLookup> + MaybeSend {
            let answer = match self
                .channels
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&jid.to_string())
                .cloned()
            {
                Some(name) => NameLookup::Found(name),
                None => NameLookup::Failed {
                    retry_after: NAME_RETRY_COOLDOWN,
                    scope: RetryScope::Chat,
                },
            };
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

    async fn drive<S: MetadataSource + ?Sized>(
        source: &S,
        store: &Arc<ChatStore>,
        signal: &ChatNameResolveSignal,
        resolver: &mut NameResolver,
        request: NameResolveRequest,
    ) {
        // The sender is kept alive in this scope so `stop.changed()`
        // never fires: dropping it would make every select return
        // immediately and abort each lookup before it runs.
        let (_guard, mut stop) = {
            let (tx, rx) = tokio::sync::watch::channel(());
            (tx, rx)
        };
        run_pass(source, store, signal, resolver, request, &mut stop).await;
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
        drive(
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
        drive(
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
        drive(
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
    /// broadcast-worthy write — and the chat stays retryable rather than
    /// filed as asked, cooling only for the failure's delay.
    #[tokio::test]
    async fn a_metadata_failure_keeps_the_fallback() {
        let store = test_store("group-failure").await;
        feed(&store, group_message(GROUP, "MSG-G2")).await;

        let source = FakeMeta::with_group(GROUP, "Trip planning");
        source.fail_groups.store(true, Ordering::Relaxed);
        let signal = ChatNameResolveSignal::new();
        let mut resolver = NameResolver::new();
        drive(
            &source,
            &store,
            &signal,
            &mut resolver,
            named_request(&[GROUP]),
        )
        .await;
        assert_eq!(stored_name(&store, GROUP).await, None);
        // Cooled, not settled: still within its delay the chat is not due
        // (no per-message hammering), and it was never marked asked.
        assert!(
            !resolver.needs(GROUP, signal.generation()),
            "a fresh cooldown holds the next message off"
        );

        // Healing the source and expiring the cooldown resolves, without a
        // reconnect in between. The fake cools for the real 60s, so the
        // test expires it the way its own expiry would — the timing half
        // is covered by `a_failed_lookup_cools_then_retries`; this half
        // covers that a failed chat IS retryable, not filed as asked.
        source.fail_groups.store(false, Ordering::Relaxed);
        resolver.expire_cooldown(GROUP);
        assert!(
            resolver.needs(GROUP, signal.generation()),
            "an expired cooldown is due again"
        );
        drive(
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

    /// A failed full-pass revalidation retries on a later sighting: the
    /// pass leaves the chat unmarked, and the sighting does not accept the
    /// stale stored name as sufficient — it looks the network up again.
    #[tokio::test]
    async fn a_failed_full_pass_retries_on_a_later_sighting() {
        let store = test_store("group-full-fail-retry").await;
        feed(&store, group_message(GROUP, "MSG-G6")).await;

        let source = FakeMeta::with_group(GROUP, "Old name");
        let signal = ChatNameResolveSignal::new();
        let mut resolver = NameResolver::new();
        // Seed the stored name through a good sighting first.
        drive(
            &source,
            &store,
            &signal,
            &mut resolver,
            named_request(&[GROUP]),
        )
        .await;
        assert_eq!(
            stored_name(&store, GROUP).await.as_deref(),
            Some("Old name")
        );

        // Offline rename the server now carries — but the reconnect's full
        // pass fails, so nothing is marked and the stale name stands.
        source.rename_group(GROUP, "New name");
        source.fail_groups.store(true, Ordering::Relaxed);
        signal.new_connection();
        let request = signal.next().await;
        assert!(request.full);
        drive(&source, &store, &signal, &mut resolver, request).await;
        assert_eq!(
            stored_name(&store, GROUP).await.as_deref(),
            Some("Old name"),
            "the failed full pass writes nothing"
        );

        // A later live message sights the chat: the resolver must NOT
        // accept the stored "Old name" as sufficient — the generation
        // owes this chat a retry. Past the cooldown, the network is tried
        // again and the rename lands.
        source.fail_groups.store(false, Ordering::Relaxed);
        resolver.expire_cooldown(GROUP);
        drive(
            &source,
            &store,
            &signal,
            &mut resolver,
            named_request(&[GROUP]),
        )
        .await;
        assert_eq!(
            stored_name(&store, GROUP).await.as_deref(),
            Some("New name"),
            "a failed revalidation retries on the next sighting past its cooldown"
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
        drive(
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
        drive(&source, &store, &signal, &mut resolver, request).await;
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
            fn group_subject(&self, jid: &Jid) -> impl Future<Output = NameLookup> + MaybeSend {
                async move {
                    self.gate.entered.notify_one();
                    self.gate.release.notified().await;
                    self.inner.group_subject(jid).await
                }
            }
            #[allow(clippy::manual_async_fn)]
            fn subscribed_channels(
                &self,
            ) -> impl Future<Output = Result<HashMap<String, String>, NameRetry>> + MaybeSend
            {
                async move { Ok(HashMap::new()) }
            }
            #[allow(clippy::manual_async_fn)]
            fn channel_name(&self, jid: &Jid) -> impl Future<Output = NameLookup> + MaybeSend {
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
            // The spawned pass must not race teardown: keep the sender
            // alive so `stop.changed()` never fires mid-lookup.
            let (_guard, mut stop) = {
                let (tx, rx) = tokio::sync::watch::channel(());
                (tx, rx)
            };
            run_pass(
                &slow,
                &pass_store,
                &pass_signal,
                &mut pass_resolver,
                named_request(&[GROUP]),
                &mut stop,
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
            drive(&source, &store, &signal, &mut resolver, request).await;
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

    /// A failed lookup cools the chat: due again after the delay, not on
    /// the next message — and the server's own backoff is what sets the
    /// length, so a throttled account does not re-request into its limit.
    #[test]
    fn a_failed_lookup_cools_then_retries() {
        // `wacore::time::Instant` is the monotonic clock the tree reads
        // everywhere (clippy bans `std::time::Instant`); it has no manual
        // advance, so the test cools with a zero delay for "already due"
        // and a day for "still quiet" rather than sleeping.
        let mut resolver = NameResolver::new();
        assert!(resolver.needs(GROUP, 1), "unasked is due");
        resolver.cool(GROUP, 1, std::time::Duration::ZERO);
        assert!(resolver.needs(GROUP, 1), "an elapsed cooldown is due again");
        resolver.cool(GROUP, 1, std::time::Duration::from_secs(86_400));
        assert!(
            !resolver.needs(GROUP, 1),
            "a fresh cooldown holds the next message off"
        );
        assert!(
            resolver.needs(GROUP, 2),
            "a new connection revalidates past any cooldown"
        );
        resolver.mark(GROUP, 1);
        assert!(!resolver.needs(GROUP, 1), "settling clears the cooldown");
    }

    /// A failed bulk list cools the channel half instead of fanning out:
    /// no per-channel request fires, nothing is settled, and a later pass
    /// past the cooldown retries the list first.
    #[tokio::test]
    async fn a_failed_channel_list_fans_out_to_nothing() {
        let store = test_store("channel-list-fails").await;
        feed(&store, group_message(CHANNEL, "MSG-C3")).await;

        // The channel HAS selective metadata ready — the point is that a
        // failed list must not reach for it.
        let source = FakeMeta::with_channel(CHANNEL, "Quiet updates", false);
        source.fail_list.store(true, Ordering::Relaxed);
        let signal = ChatNameResolveSignal::new();
        let mut resolver = NameResolver::new();
        drive(
            &source,
            &store,
            &signal,
            &mut resolver,
            named_request(&[CHANNEL]),
        )
        .await;

        assert_eq!(
            stored_name(&store, CHANNEL).await,
            None,
            "a failed list must not fan out into selective lookups"
        );
        assert!(
            !resolver.needs(CHANNEL, signal.generation()),
            "the failed list cools the chat"
        );

        // Healed and expired, the retry goes through the list first: the
        // channel is listed this time and resolves without any fallback.
        let source = FakeMeta::with_channel(CHANNEL, "Announcements", true);
        resolver.expire_cooldown(CHANNEL);
        drive(
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
        drive(
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
