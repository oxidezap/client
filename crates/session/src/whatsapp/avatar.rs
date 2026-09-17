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
/// The distinction is why this is not an `Option`: the library used to fold
/// "no picture" (`404`), "not authorized" (`401`), "unchanged" (`304`) and a
/// partial response into one `Ok(None)`, and only a positive answer may change
/// what a chat shows. `NotFound` is the one destructive state, and it is now
/// told apart from the rest.
enum Lookup {
    /// A picture with a fetchable source.
    Found { picture_id: String, source: String },
    /// The picture did not change, or the answer carried no usable URL.
    /// Nothing to fetch, nothing to forget.
    Unchanged,
    /// WhatsApp says this chat has no picture. The only destructive answer.
    NotFound,
    /// A refusal, a rate limit, or a partial response: the previous picture
    /// stays, because neither is evidence the picture is gone.
    Unknown,
    /// The lookup failed. Deliberately not remembered, so a transient failure
    /// is retried the next time a page names the chat.
    Failed,
}

impl Lookup {
    /// The group query's outcome, plus the community fallback.
    ///
    /// A community parent is an ordinary `@g.us` address, so the JID cannot
    /// say which query it needs; only group metadata's `is_parent_group` can,
    /// and fetching that would be an extra IQ per group on every connect. So
    /// the community query is a *fallback*, taken when the group query answers
    /// `NotAuthorized`: a parent refuses `w:profile:picture` and answers the
    /// `w:g2` query, while a normal group that is merely privacy-restricted
    /// refuses both.
    ///
    /// Only `Found` is taken from the fallback. A non-parent can answer the
    /// community query with a `404`, and trusting that would erase a picture
    /// the ordinary query refused to show rather than said was gone — the
    /// destructive reading this whole type exists to avoid.
    fn of(outcome: whatsapp_rust::features::ProfilePictureLookup) -> Self {
        use whatsapp_rust::features::ProfilePictureLookup as Outcome;
        match outcome {
            Outcome::Found(picture) if !picture.url.is_empty() => Self::Found {
                picture_id: picture.id,
                source: picture.url,
            },
            // An id with no URL: the metadata moved but there is nothing to
            // fetch, so this session keeps what it had.
            Outcome::Found(_) | Outcome::Unchanged => Self::Unchanged,
            Outcome::NotFound => Self::NotFound,
            // Not authorized and rate-limited say nothing about whether the
            // picture exists, so they must not erase one that does.
            //
            // The wildcard is `#[non_exhaustive]`'s: a state this build does
            // not know about is read as "nothing definite" rather than as the
            // one destructive answer, because guessing "removed" for an
            // unknown state is the mistake this type exists to prevent.
            Outcome::NotAuthorized | Outcome::RateOverlimit | _ => Self::Unknown,
        }
    }

    /// Whether the fallback is worth asking, and only it.
    ///
    /// The one outcome a community parent is known to produce for the wrong
    /// query. `RateOverlimit` is deliberately excluded: it is about this
    /// client's request rate, not about the entity, and asking again would
    /// spend another request against a limit already reached.
    fn wants_community_fallback(&self) -> bool {
        matches!(self, Self::Unknown)
    }

    /// Read the fallback's answer, keeping only a picture.
    ///
    /// Anything else leaves the original answer standing.
    fn or_community(self, outcome: whatsapp_rust::features::ProfilePictureLookup) -> Self {
        match Self::of(outcome) {
            found @ Self::Found { .. } => found,
            // Unchanged is not evidence of anything here either: the fallback
            // never sent an `existing_id`, so there is nothing for it to be
            // unchanged *against*.
            _ => self,
        }
    }
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
/// The boolean tracks whether bytes were requested (`had_bytes = true`) or only
/// metadata freshness checked (`had_bytes = false`).
#[derive(Default)]
pub(super) struct Resolver {
    asked: HashMap<String, (u64, bool)>,
}

impl Resolver {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Whether `jid` still needs a lookup in this generation.
    fn needs(&self, jid: &str, generation: u64, need_bytes: bool) -> bool {
        match self.asked.get(jid) {
            None => true,
            Some(&(recorded_gen, had_bytes)) => {
                if recorded_gen != generation {
                    true
                } else if need_bytes && !had_bytes {
                    // Previous ask was freshness-only, but caller needs bytes.
                    true
                } else {
                    false
                }
            }
        }
    }

    fn mark_asked(&mut self, jid: &str, generation: u64, had_bytes: bool) {
        let entry = self
            .asked
            .entry(jid.to_string())
            .or_insert((generation, had_bytes));
        if entry.0 != generation {
            *entry = (generation, had_bytes);
        } else if had_bytes {
            entry.1 = true;
        }
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
/// The group branch carries the community fallback; see [`Lookup::or_community`]
/// for why only a `Found` is taken from it.
///
/// When `need_bytes` is true, `existing_id` is passed as `None` to avoid the
/// "unchanged trap": an `Unchanged` response carries no download URL, so if
/// cached bytes are missing, an unconditional lookup is required.
async fn lookup(
    client: &Arc<Client>,
    jid: &Jid,
    known_id: Option<&str>,
    need_bytes: bool,
) -> Lookup {
    let existing_id = if need_bytes { None } else { known_id };
    if jid.is_group() {
        let asked = match client
            .groups()
            .lookup_profile_picture(jid, true, existing_id)
            .await
        {
            Ok(outcome) => Lookup::of(outcome),
            Err(error) => {
                debug!(
                    "avatar metadata lookup failed for {}: {error}",
                    jid.observe()
                );
                return Lookup::Failed;
            }
        };
        if !asked.wants_community_fallback() {
            return asked;
        }
        // Not authorized, which for a community parent is what
        // `w:profile:picture` answers. Ask the `w:g2` query before giving up.
        match client
            .groups()
            .lookup_community_profile_picture(jid, true, existing_id)
            .await
        {
            Ok(outcome) => asked.or_community(outcome),
            // A group that is not a parent has no answer here; the original
            // not-authorized stands, and it was not destructive.
            Err(_) => asked,
        }
    } else {
        match client
            .contacts()
            .lookup_profile_picture(jid, true, existing_id)
            .await
        {
            Ok(outcome) => Lookup::of(outcome),
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
}

/// Resolve avatar demands and publish what was learned, a chunk at a time.
///
/// Bounded and deduplicated. The caller is viewport demand or an explicit ask.
pub(super) async fn resolve(
    client: &Arc<Client>,
    ui_tx: &UiEventSender,
    resolver: &mut Resolver,
    demands: Vec<oxidezap_core::AvatarDemand>,
    generation: u64,
) {
    let mut filtered: HashMap<String, (Jid, Option<String>, bool)> = HashMap::new();
    for demand in demands {
        let Ok(jid) = demand.jid.parse::<Jid>() else {
            continue;
        };
        if lookup_is_useless(&jid) {
            continue;
        }
        if !resolver.needs(&demand.jid, generation, demand.need_bytes) {
            continue;
        }
        let entry = filtered
            .entry(demand.jid.clone())
            .or_insert_with(|| (jid, demand.known_picture_id.clone(), demand.need_bytes));
        if demand.need_bytes {
            entry.2 = true;
        }
    }

    let pending: Vec<(Jid, Option<String>, bool)> = filtered.into_values().collect();
    if pending.is_empty() {
        return;
    }
    let total = pending.len();
    let mut published = 0usize;
    for chunk in pending.chunks(AVATAR_CONCURRENCY) {
        let answered = whatsapp_rust::futures::future::join_all(chunk.iter().map(
            |(jid, known_id, need_bytes)| {
                let jid = jid.clone();
                let known_id = known_id.clone();
                let need_bytes = *need_bytes;
                async move {
                    let answer = lookup(client, &jid, known_id.as_deref(), need_bytes).await;
                    (jid, need_bytes, answer)
                }
            },
        ))
        .await;

        let mut resolutions = Vec::with_capacity(answered.len());
        for (jid, _need_bytes, answer) in answered {
            let jid_str = jid.to_string();
            let outcome = match answer {
                Lookup::Found { picture_id, source } => oxidezap_core::AvatarOutcome::Found {
                    picture_id,
                    source: Some(source),
                },
                Lookup::NotFound => oxidezap_core::AvatarOutcome::NotFound,
                // An answer with nothing to act on: unchanged, a refusal, a
                // rate limit, a partial response. Remembered as asked so the
                // next page does not repeat the request, and not published,
                // because nothing on the other side would do anything with it.
                Lookup::Unchanged | Lookup::Unknown => {
                    resolver.mark_asked(&jid_str, generation, false);
                    continue;
                }
                // A failure is deliberately not remembered, so a transient one
                // is retried the next time a demand names the chat.
                Lookup::Failed => continue,
            };
            resolutions.push(oxidezap_core::AvatarResolution {
                jid: jid_str,
                outcome,
            });
        }
        if resolutions.is_empty() {
            continue;
        }
        for resolution in &resolutions {
            let had_bytes = matches!(resolution.outcome, oxidezap_core::AvatarOutcome::NotFound);
            resolver.mark_asked(&resolution.jid, generation, had_bytes);
        }
        let count = resolutions.len();
        match ui_tx.send(UiEvent::AvatarsResolved { resolutions }) {
            Ok(()) => {
                published += count;
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
                if !request.demands.is_empty() {
                    resolve(
                        &client,
                        &ui_tx,
                        &mut resolver,
                        request.demands,
                        request.generation,
                    )
                    .await;
                }
                if request.full {
                    let jids = stored_jids(&chat_store).await;
                    let demands = jids
                        .into_iter()
                        .map(|jid| oxidezap_core::AvatarDemand {
                            jid: jid.to_string(),
                            known_picture_id: None,
                            cache_key: None,
                            need_bytes: true,
                        })
                        .collect();
                    resolve(&client, &ui_tx, &mut resolver, demands, request.generation).await;
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
        assert!(resolver.needs("a@s.whatsapp.net", 1, true));
        resolver.mark_asked("a@s.whatsapp.net", 1, true);
        assert!(!resolver.needs("a@s.whatsapp.net", 1, true));
        assert!(resolver.needs("b@s.whatsapp.net", 1, true));

        resolver.forget_all();
        assert!(
            resolver.needs("a@s.whatsapp.net", 1, true),
            "a clear re-queries"
        );
    }

    /// A reconnect is a new generation, and every previous answer is stale:
    /// a picture can change while the process is offline.
    #[test]
    fn a_new_connection_revalidates_what_the_last_one_resolved() {
        let mut resolver = Resolver::new();
        resolver.mark_asked("a@s.whatsapp.net", 1, true);
        assert!(!resolver.needs("a@s.whatsapp.net", 1, true));
        assert!(
            resolver.needs("a@s.whatsapp.net", 2, true),
            "the answer belongs to the socket that gave it"
        );
    }

    /// Needing bytes supersedes a previous freshness-only ask.
    #[test]
    fn needing_bytes_supersedes_a_metadata_only_ask() {
        let mut resolver = Resolver::new();
        resolver.mark_asked("a@s.whatsapp.net", 1, false);
        assert!(!resolver.needs("a@s.whatsapp.net", 1, false));
        assert!(resolver.needs("a@s.whatsapp.net", 1, true));
    }

    /// A metadata-only Unchanged lookup records had_bytes: false, so a subsequent byte demand proceeds.
    #[test]
    fn metadata_only_unchanged_lookup_permits_subsequent_byte_demand() {
        let mut resolver = Resolver::new();
        // Unchanged on metadata-only ask records had_bytes = false:
        resolver.mark_asked("a@s.whatsapp.net", 1, false);
        assert!(!resolver.needs("a@s.whatsapp.net", 1, false));
        assert!(
            resolver.needs("a@s.whatsapp.net", 1, true),
            "byte demand must proceed even after metadata Unchanged"
        );
    }
}
