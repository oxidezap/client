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
//! What this publishes is [`UiEvent::AvatarsResolved`], which carries the
//! picture id and the signed source the daemon fetches, and never the bytes
//! or the URL in state.

use std::collections::HashMap;
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

/// What one metadata lookup actually told us.
///
/// The distinction matters because the library folds four different answers
/// into `Ok(None)`: no picture, an unchanged picture (`304`), a partial
/// response with no usable URL, and *not authorized* (`401`). Only a positive
/// answer may change what a chat shows; an ambiguous one must leave a known
/// avatar alone, or a privacy refusal would erase a valid picture. That is
/// also why there is no "removed" arm: without a definitive signal from the
/// library, treating a `None` as removal is the destructive reading this
/// avoids. A removed picture therefore keeps showing until the picture id
/// changes or the cache is cleared.
enum Lookup {
    /// A picture with a fetchable source.
    Found { picture_id: String, source: String },
    /// An answer that says nothing definite. Counted as asked, changes
    /// nothing.
    Unknown,
    /// The lookup failed. Deliberately not remembered, so a transient failure
    /// is retried the next time a page names the chat.
    Failed,
}

/// Chats whose metadata this side has already asked about, and when.
///
/// What makes the resolver safe to ask again on every page: a chat already
/// asked about costs a hash lookup rather than an IQ, so a page that repeats
/// its rows does not repeat its traffic.
///
/// The generation is the connection it was resolved under. A new connection
/// makes every entry stale, because a picture can change while the process is
/// offline and the previous socket's answer says nothing about this one.
#[derive(Default)]
pub(super) struct Resolver {
    asked: HashMap<String, u64>,
}

impl Resolver {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Whether `jid` still needs a metadata lookup in this generation.
    fn needs(&self, jid: &str, generation: u64) -> bool {
        self.asked.get(jid) != Some(&generation)
    }

    fn mark_asked(&mut self, jid: &str, generation: u64) {
        self.asked.insert(jid.to_string(), generation);
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
///
/// What this side *does* have to add is the reading of `Ok(None)`, which is
/// not an answer about removal. See [`Lookup`].
async fn lookup(client: &Arc<Client>, jid: &Jid) -> Lookup {
    match client.contacts().get_profile_picture(jid, true).await {
        Ok(Some(picture)) => match picture.url.is_empty() {
            // A picture id with no URL: the metadata moved but there is
            // nothing to fetch, so this session keeps what it had.
            true => Lookup::Unknown,
            false => Lookup::Found {
                picture_id: picture.id,
                source: picture.url,
            },
        },
        Ok(None) => Lookup::Unknown,
        Err(error) => {
            // Facts only: never the URL or any token it carries.
            debug!(
                "avatar metadata lookup failed for {}: {error}",
                jid.observe()
            );
            Lookup::Failed
        }
    }
}

/// Resolve a page of chats and publish what was learned, a chunk at a time.
///
/// Bounded and deduplicated. The caller is the connect path or a chat-list
/// page; a receipt is never one.
///
/// Published per chunk rather than once at the end: the first eight pictures
/// are worth drawing while the next eight are in flight, and holding a whole
/// five-hundred-chat pass behind its slowest request would leave the visible
/// list on placeholders for no reason.
pub(super) async fn resolve(
    client: &Arc<Client>,
    ui_tx: &UiEventSender,
    resolver: &mut Resolver,
    jids: Vec<Jid>,
    generation: u64,
) {
    let pending: Vec<Jid> = jids
        .into_iter()
        .filter(|jid| !lookup_is_useless(jid))
        .filter(|jid| resolver.needs(&jid.to_string(), generation))
        .collect();
    if pending.is_empty() {
        return;
    }
    let total = pending.len();
    let mut published = 0usize;
    for chunk in pending.chunks(AVATAR_CONCURRENCY) {
        let answered = whatsapp_rust::futures::future::join_all(
            chunk
                .iter()
                .map(|jid| async move { (jid.clone(), lookup(client, jid).await) }),
        )
        .await;

        let mut resolutions = Vec::with_capacity(answered.len());
        for (jid, answer) in answered {
            match answer {
                Lookup::Found { picture_id, source } => {
                    resolutions.push(oxidezap_core::AvatarResolution {
                        jid: jid.to_string(),
                        picture_id,
                        source: Some(source),
                    })
                }
                // An answer, if not a useful one: remembered so it is not
                // asked again this connection.
                Lookup::Unknown => resolver.mark_asked(&jid.to_string(), generation),
                Lookup::Failed => {}
            }
        }
        if resolutions.is_empty() {
            continue;
        }
        // One event for the chunk, not one per chat: the queue between here
        // and the daemon is bounded, and a first connect resolving a large
        // account would otherwise drop most of its own answers as overflow.
        //
        // The send is the line between "asked" and "answered": a chunk the
        // queue refused is left unmarked, so the next pass asks again rather
        // than believing a lookup that never reached the daemon.
        let chats: Vec<String> = resolutions
            .iter()
            .map(|resolution| resolution.jid.clone())
            .collect();
        match ui_tx.send(UiEvent::AvatarsResolved { resolutions }) {
            Ok(()) => {
                for jid in &chats {
                    resolver.mark_asked(jid, generation);
                }
                published += chats.len();
            }
            Err(_) => return,
        }
    }
    debug!("avatar resolver: {published} of {total} chats published a picture");
}

/// How many chats one full pass reads.
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
                let request = tokio::select! {
                    request = signal.next() => request,
                    _ = stopping.changed() => return,
                };
                if request.reset {
                    resolver.forget_all();
                }
                if !request.named.is_empty() {
                    // The named chats are what somebody just looked at, so
                    // they are resolved first and on their own: the store
                    // window a full pass reads may not even hold them.
                    let named: Vec<Jid> = request
                        .named
                        .iter()
                        .filter_map(|jid| jid.parse().ok())
                        .collect();
                    resolve(&client, &ui_tx, &mut resolver, named, request.generation).await;
                }
                // A full ask covers the store's window; a named-only ask has
                // already been answered above.
                if request.full {
                    let jids = stored_jids(&chat_store).await;
                    resolve(&client, &ui_tx, &mut resolver, jids, request.generation).await;
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
        assert!(resolver.needs("a@s.whatsapp.net", 1));
        resolver.mark_asked("a@s.whatsapp.net", 1);
        assert!(!resolver.needs("a@s.whatsapp.net", 1));
        assert!(resolver.needs("b@s.whatsapp.net", 1));

        resolver.forget_all();
        assert!(resolver.needs("a@s.whatsapp.net", 1), "a clear re-queries");
    }

    /// A reconnect is a new generation, and every previous answer is stale:
    /// a picture can change while the process is offline.
    #[test]
    fn a_new_connection_revalidates_what_the_last_one_resolved() {
        let mut resolver = Resolver::new();
        resolver.mark_asked("a@s.whatsapp.net", 1);
        assert!(!resolver.needs("a@s.whatsapp.net", 1));
        assert!(
            resolver.needs("a@s.whatsapp.net", 2),
            "the answer belongs to the socket that gave it"
        );
    }
}
