//! Resolving profile pictures, on their own lifecycle.
//!
//! A picture is relatively stable metadata, and it used to be looked up by the
//! history reload path: every store invalidation, every receipt and every
//! acknowledgement ran the same profile-picture IQs over the same chats. That
//! coupled a slow network round trip to a local read, delayed `HistoryLoaded`
//! behind it, and paid for a hundred lookups to learn that nothing had moved.
//!
//! So the lookup lives here instead, driven by two things: one pass after the
//! session reports `Connected`, and one when the chat list is paged further.
//! A receipt or an ack starts nothing.
//!
//! Only the *metadata* is fetched here; the bytes are the daemon's business.
//! What this publishes is [`UiEvent::AvatarResolved`], which carries the
//! picture id and the signed source the daemon fetches, and never the bytes
//! or the URL in state.

use std::collections::HashSet;
use std::sync::Arc;

use log::debug;
use oxidezap_chat_store::ChatStore;
use oxidezap_core::UiEvent;
use whatsapp_rust::client::Client;
use whatsapp_rust::wacore_binary::jid::{Jid, JidExt};

use super::WhatsAppClient;
use super::ui_queue::Sender as UiEventSender;

/// How many avatar lookups may be in flight at once.
///
/// A per-chat IQ, so the limit is what keeps a first connect on an account of
/// a thousand chats from opening a thousand of them. Eight is comfortably
/// inside what the server tolerates while still filling a burst quickly.
const AVATAR_CONCURRENCY: usize = 8;

/// Chats whose metadata this side has already asked about.
///
/// What makes the resolver safe to ask again on every page: a chat already
/// asked about costs a hash lookup rather than an IQ, so a page that repeats
/// its rows does not repeat its traffic. A transient failure is deliberately
/// *not* remembered, so the next page retries it.
#[derive(Default)]
pub(super) struct Resolver {
    asked: HashSet<String>,
}

impl Resolver {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Whether `jid` still needs a metadata lookup.
    fn needs(&self, jid: &str) -> bool {
        !self.asked.contains(jid)
    }

    fn mark_asked(&mut self, jid: &str) {
        self.asked.insert(jid.to_string());
    }

    /// Forget every chat, so the next pass re-queries.
    ///
    /// For a cache that was cleared or an account change: the metadata may be
    /// unchanged, but the source it described has expired and the bytes it
    /// named are gone.
    pub(super) fn forget_all(&mut self) {
        self.asked.clear();
    }
}

/// The JID kinds this side will not spend an IQ on.
///
/// The status broadcast is a pseudo-user that never answers, a broadcast list
/// is a local construct with no server picture, and the system account is
/// short-circuited by the library. Asking for any is a request timeout spent
/// to learn nothing.
fn lookup_is_useless(jid: &Jid) -> bool {
    jid.is_status_broadcast() || jid.is_broadcast_list() || jid.is_psa()
}

/// Fetch one chat's picture metadata.
///
/// The generic `ProfilePictureSpec` path handles ordinary contacts, groups,
/// newsletters and bots alike: the library already skips the privacy-token
/// dance for everything that is not a plain PN, so this needs no per-kind
/// branch to be correct.
async fn lookup(client: &Arc<Client>, jid: &Jid) -> Option<(String, Option<String>)> {
    match client.contacts().get_profile_picture(jid, true).await {
        Ok(Some(picture)) => Some((picture.id, Some(picture.url))),
        // A chat with no picture. A result rather than a failure, so it is
        // remembered and never asked again.
        Ok(None) => Some((String::new(), None)),
        Err(error) => {
            // Facts only: never the URL or any token it carries.
            debug!(
                "avatar metadata lookup failed for {}: {error}",
                jid.observe()
            );
            None
        }
    }
}

/// Resolve a page of chats and publish what was learned.
///
/// Bounded and deduplicated. The caller is the connect path or a chat-list
/// page; a receipt is never one.
pub(super) async fn resolve(
    client: &Arc<Client>,
    ui_tx: &UiEventSender,
    resolver: &mut Resolver,
    jids: Vec<Jid>,
) {
    let pending: Vec<Jid> = jids
        .into_iter()
        .filter(|jid| !lookup_is_useless(jid))
        .filter(|jid| resolver.needs(&jid.to_string()))
        .collect();
    if pending.is_empty() {
        return;
    }
    let total = pending.len();
    let mut resolved = Vec::with_capacity(total);
    for chunk in pending.chunks(AVATAR_CONCURRENCY) {
        let pictures = whatsapp_rust::futures::future::join_all(
            chunk
                .iter()
                .map(|jid| async move { (jid.clone(), lookup(client, jid).await) }),
        )
        .await;
        resolved.extend(pictures);
    }
    let mut published = 0usize;
    let mut resolutions = Vec::with_capacity(resolved.len());
    for (jid, picture) in resolved {
        let Some((picture_id, source)) = picture else {
            // A failure is not remembered, so a transient one is retried the
            // next time a page names the chat.
            continue;
        };
        resolver.mark_asked(&jid.to_string());
        resolutions.push(oxidezap_core::AvatarResolution {
            jid: jid.to_string(),
            picture_id,
            source,
        });
        published += 1;
    }
    if resolutions.is_empty() {
        return;
    }
    // One event for the batch, not one per chat: the queue between here and
    // the daemon is bounded, and a first connect resolving a large account
    // would otherwise drop most of its own answers as overflow.
    if ui_tx
        .send(UiEvent::AvatarsResolved { resolutions })
        .is_err()
    {
        return;
    }
    debug!("avatar resolver: {published} of {total} chats published a picture");
}

/// How many chats one resolver pass reads.
///
/// The chat list is drawn from a hundred at a time, and the descriptor attach
/// runs over exactly that page; a pass that covered every chat in a very
/// large account would spend a lookup per conversation to learn what the next
/// page's attach will not even draw yet. Five pages is enough to cover the
/// visible list and the near tail, and the next connect covers any more.
const RESOLVE_CHAT_LIMIT: i64 = 500;

/// The addresses to resolve for the chats the store holds.
pub(super) async fn stored_jids(chat_store: &Arc<ChatStore>) -> Vec<Jid> {
    match chat_store.chats(false, RESOLVE_CHAT_LIMIT).await {
        Ok(entries) => entries.into_iter().map(|entry| entry.jid).collect(),
        Err(e) => {
            debug!("avatar resolver could not read the chat list: {e}");
            Vec::new()
        }
    }
}

/// The resolver's whole life: wait for a pass, resolve, repeat.
///
/// One task for the session, like the history reloader, and for the same
/// reason: profile-picture lookups are a stream of the account's own metadata
/// and there is no sense in a task per ask. Stops when the session does.
impl WhatsAppClient {
    pub(super) fn spawn_avatar_resolver(
        client: Arc<Client>,
        chat_store: Arc<ChatStore>,
        ui_tx: UiEventSender,
        signal: super::AvatarResolveSignal,
        stopping: tokio::sync::watch::Receiver<()>,
    ) {
        crate::exec::spawn_owned(async move {
            let mut resolver = Resolver::new();
            let mut stopping = stopping;
            loop {
                let (reset, named) = tokio::select! {
                    awaited = signal.next() => awaited,
                    _ = stopping.changed() => return,
                };
                if reset {
                    resolver.forget_all();
                }
                if !named.is_empty() {
                    // The named chats are what somebody just looked at, so
                    // they are resolved first and on their own: the store
                    // window the full pass reads may not even hold them.
                    let named: Vec<Jid> = named.iter().filter_map(|jid| jid.parse().ok()).collect();
                    resolve(&client, &ui_tx, &mut resolver, named).await;
                }
                // A plain ask and a reset both cover the store's window; a
                // named-only ask has already been answered above.
                if reset || named.is_empty() {
                    let jids = stored_jids(&chat_store).await;
                    resolve(&client, &ui_tx, &mut resolver, jids).await;
                }
            }
        });
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_broadcast_is_never_looked_up() {
        assert!(lookup_is_useless(&"status@broadcast".parse().unwrap()));
        assert!(lookup_is_useless(&"0@s.whatsapp.net".parse().unwrap()));
    }

    /// A channel is not the accidental `!is_group` branch. Its JID is a
    /// newsletter, the generic profile-picture spec answers it, and the only
    /// thing this side decides is that it is worth asking.
    #[test]
    fn a_channel_is_worth_asking_about() {
        let newsletter: Jid = "120363000000000001@newsletter".parse().unwrap();
        let group: Jid = "120363000000000002@g.us".parse().unwrap();
        let direct: Jid = "12025550143@s.whatsapp.net".parse().unwrap();
        assert!(!lookup_is_useless(&newsletter));
        assert!(!lookup_is_useless(&group));
        assert!(!lookup_is_useless(&direct));
        assert!(newsletter.is_newsletter());
    }

    /// The dedup is what keeps a repeated page from repeating its traffic.
    #[test]
    fn an_asked_chat_is_not_asked_again() {
        let mut resolver = Resolver::new();
        assert!(resolver.needs("a@s.whatsapp.net"));
        resolver.mark_asked("a@s.whatsapp.net");
        assert!(!resolver.needs("a@s.whatsapp.net"));
        assert!(resolver.needs("b@s.whatsapp.net"));

        resolver.forget_all();
        assert!(resolver.needs("a@s.whatsapp.net"), "a clear re-queries");
    }
}
