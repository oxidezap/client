//! Store rows read back as the chats a front end draws.
//!
//! The attach load, the reloader that answers store invalidations, and the
//! per-chat work both of them share with a page of the chat list: the PN/LID
//! collapse, reactions, sender names, the unread tail and the preview. They
//! are one file because every one of them is a decision about how much of the
//! store a single answer is allowed to cost — history is asked for rather than
//! pushed, a load is read in pages rather than in rows, and the debounce in
//! front of the reloader is there for bursts and not for askers.

use std::collections::HashSet;
use std::sync::Arc;

use log::warn;
use oxidezap_chat_store::{ChatEntry, ChatStore, StoreChange};
use tokio::sync::mpsc;
use whatsapp_rust::bot::Bot;
use whatsapp_rust::client::Client;
use whatsapp_rust::wacore_binary::jid::{Jid, observe_str};

use oxidezap_core::{Chat, ChatMessage, UiEvent};

use super::WhatsAppClient;
use super::convert::{mark_unread_tail, stored_to_chat_message};
use super::paging::chat_cursor;
use crate::names::NameBook;

/// What a history load has to say: the chats, whether they are the whole
/// list, and where the list continues.
///
/// The third is what makes a front end's first "load more" a page it does not
/// already have. A load has already walked the store's order to its limit, so
/// the position it stopped at costs nothing to carry; asking for it instead
/// is a hundred rows re-read, re-serialized and re-merged to learn one
/// string.
pub(super) struct LoadedHistory {
    pub(super) chats: Vec<oxidezap_core::Chat>,
    pub(super) complete: bool,
    pub(super) next: Option<String>,
}

impl LoadedHistory {
    /// The event a front end reads this as. One place, so a load cannot reach
    /// a window having quietly dropped where it ended.
    pub(super) fn into_event(self) -> UiEvent {
        UiEvent::HistoryLoaded {
            chats: self.chats,
            complete: self.complete,
            next: self.next,
        }
    }
}

/// What a debounced window of store invalidations forces a reload to cover.
///
/// The store names the chat behind every message-level change, and a change
/// confined to message rows leaves the list's order, membership and names
/// alone — so the window can be answered by rebuilding just those chats.
/// Anything else in the window (or a gap in it) widens the reload back to the
/// whole list, because that is the only load allowed to prune.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ReloadScope {
    /// Only these chats' message sets moved.
    Chats(HashSet<String>),
    /// Rebuild the display list.
    Everything,
}

impl ReloadScope {
    pub(super) fn empty() -> Self {
        ReloadScope::Chats(HashSet::new())
    }

    /// Fold one invalidation in. `None` is a lagged receiver: what it dropped
    /// is unknowable, so it counts as everything.
    pub(super) fn widen(&mut self, change: Option<&StoreChange>) {
        match (&mut *self, change) {
            (ReloadScope::Everything, _) => {}
            // Contacts too: a push name landing after the chat row must
            // refresh chats stuck on the JID placeholder, and naming is
            // resolved for the whole list at load time.
            (_, None) | (_, Some(StoreChange::Chats | StoreChange::Contacts)) => {
                *self = ReloadScope::Everything;
            }
            (ReloadScope::Chats(chats), Some(StoreChange::Messages { chat })) => {
                chats.insert(chat.to_non_ad_string());
            }
        }
    }

    /// The chats to rebuild, or `None` for the whole list.
    pub(super) fn chats(&self) -> Option<&HashSet<String>> {
        match self {
            ReloadScope::Chats(chats) => Some(chats),
            ReloadScope::Everything => None,
        }
    }
}

impl WhatsAppClient {
    pub(super) const HISTORY_CHAT_LIMIT: i64 = 100;
    /// How many of a chat's newest messages the attach load carries.
    ///
    /// Not a timeline — a front end asks for that when it has somewhere to
    /// draw it. What stays is what this side needs to do its own job: the
    /// newest row, which the chat list draws its preview from, and the unread
    /// tail, which is the set of receipts a read owes and the second a read is
    /// bounded by. A chat nobody has an unread message in needs almost
    /// nothing; the floor is there so an ordinary same-second burst is
    /// covered rather than truncated.
    const ATTACH_FLOOR: i64 = 8;
    /// And no more than a page, however many are unread: past that the front
    /// end is asking for history anyway.
    const ATTACH_CEILING: i64 = 50;
    /// Quiet window before reloading: one history-sync chunk commits as many
    /// write batches, each emitting a change; reload once per burst.
    const RELOAD_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

    /// One task for the whole session: chat-store invalidations -> debounced
    /// load_history -> HistoryLoaded.
    ///
    /// Exits when the session does, which is `stopping` and not the store: it
    /// holds an `Arc<ChatStore>` itself, and that store owns the sender its
    /// receiver is waiting on — so "the store went away" is a thing this task
    /// makes impossible by existing. On a desktop that never showed, because
    /// dropping the runtime took the task with it; on a page nothing does.
    pub(super) fn spawn_history_reloader(
        mut changes: tokio::sync::broadcast::Receiver<oxidezap_chat_store::StoreChange>,
        chat_store: Arc<ChatStore>,
        bot: &Bot,
        ui_tx: &mpsc::UnboundedSender<UiEvent>,
        reload: Arc<tokio::sync::Notify>,
        names: Arc<NameBook>,
        mut stopping: tokio::sync::watch::Receiver<()>,
    ) {
        use tokio::sync::broadcast::error::RecvError;

        let client = bot.client();
        let ui_tx = ui_tx.clone();
        crate::exec::spawn_owned(async move {
            let mut open = true;
            while open {
                let mut scope = ReloadScope::empty();
                // Either a store change or somebody asking outright. An
                // explicit ask widens to everything, because the asker is a
                // front end that has just attached and holds nothing.
                let mut asked = false;
                tokio::select! {
                    change = changes.recv() => match change {
                        Ok(change) => scope.widen(Some(&change)),
                        Err(RecvError::Lagged(_)) => scope.widen(None),
                        Err(RecvError::Closed) => break,
                    },
                    () = reload.notified() => {
                        scope.widen(None);
                        asked = true;
                    }
                    // Without a final load: the session is going, and a
                    // reload would read a store that is about to be deleted
                    // and publish it at a front end that has already left.
                    _ = stopping.changed() => break,
                }
                // Drain the burst; a quiet window flushes the reload.
                //
                // Not entered, and broken out of, when somebody asks outright.
                // The debounce is there to fold a history sync's many
                // committed batches into one load, and the cost of folding is
                // a fifth of a second before the first query runs. A front end
                // that has just attached is holding nothing and is the one
                // caller that waits on this — and there is nothing to
                // coalesce for it, because it asked for everything.
                //
                // The ask has to be watched *here* as well as in the select
                // above, not only skipped when it happened to win it: during a
                // history sync the changes never stop arriving, so a drain
                // that waits on them alone has no quiet window to end on and
                // the asker waits out the whole sync.
                while !asked {
                    tokio::select! {
                        _ = stopping.changed() => return,
                        change = crate::exec::with_timeout(changes.recv(), Self::RELOAD_DEBOUNCE) => {
                            match change {
                                Some(Ok(change)) => scope.widen(Some(&change)),
                                Some(Err(RecvError::Lagged(_))) => scope.widen(None),
                                Some(Err(RecvError::Closed)) => {
                                    // Reload once more: these changes were committed.
                                    open = false;
                                    break;
                                }
                                // The quiet window: flush what has piled up.
                                None => break,
                            }
                        }
                        () = reload.notified() => {
                            scope.widen(None);
                            asked = true;
                        }
                    }
                }
                // An empty COMPLETE load still goes out: the UI prunes
                // against the loaded set, so deleting/archiving the last chat
                // elsewhere must clear the list here too. An empty narrowed
                // one names nothing the list shows (an archived chat, or one
                // past the window) and has nothing to say.
                match Self::load_history_scoped(&chat_store, &client, scope.chats(), &names).await {
                    Ok(loaded) if loaded.chats.is_empty() && !loaded.complete => {}
                    Ok(loaded) => {
                        if ui_tx.send(loaded.into_event()).is_err() {
                            break;
                        }
                    }
                    Err(e) => warn!("failed to reload history after store change: {e}"),
                }
            }
        });
    }

    /// Build the UI chat list from the durable store: chats in display order,
    /// each with its most recent page of messages. Media bodies are not
    /// hydrated here (the proto is in the store; download stays on demand).
    /// The returned flag says whether this is the store's WHOLE display list;
    /// it comes from the raw entry count, since PN/LID collapsing can shrink
    /// a truncated fetch back under the limit.
    pub(super) async fn load_history(
        chat_store: &Arc<ChatStore>,
        client: &Arc<Client>,
        names: &NameBook,
    ) -> Result<LoadedHistory, oxidezap_chat_store::ChatStoreError> {
        Self::load_history_scoped(chat_store, client, None, names).await
    }

    /// [`load_history`](Self::load_history), restricted to the chats `only`
    /// names when it names any.
    ///
    /// The whole-list rebuild is what every invalidation used to cost: one
    /// message page, its reactions and its sender names per chat, for all of
    /// them, and receipts alone fire it several times per sent message. A
    /// receipt or an ack moves rows inside one conversation and leaves the
    /// list's order, membership and names exactly as they were, so the load
    /// it triggers can be that conversation's.
    pub(super) async fn load_history_scoped(
        chat_store: &Arc<ChatStore>,
        client: &Arc<Client>,
        only: Option<&HashSet<String>>,
        names: &NameBook,
    ) -> Result<LoadedHistory, oxidezap_chat_store::ChatStoreError> {
        // A whole-list load is the pass that re-reads the address book, so it
        // is the one that drops what the book remembers: a contact renamed on
        // the phone appears under its new name without a restart, and the
        // scoped loads in between — which run per receipt — still pay nothing.
        if only.is_none() {
            names.forget();
        }
        let mut entries = chat_store.chats(false, Self::HISTORY_CHAT_LIMIT).await?;
        // A narrowed load says nothing about the chats it left out, so it is
        // never the whole display list and must never drive the UI's prune.
        let complete = only.is_none() && (entries.len() as i64) < Self::HISTORY_CHAT_LIMIT;
        // Where this load stopped, so the front end's first "load more" is a
        // page it does not have rather than the page it was just handed. Taken
        // from the raw entries, before the alias filter below and before the
        // PN/LID collapse: a cursor is a position in the store's own order,
        // and both of those change what the list looks like without moving
        // that position. A complete load has nothing after it, and a narrowed
        // one is not a position in the list at all.
        let next = (only.is_none() && !complete)
            .then(|| entries.last().map(chat_cursor))
            .flatten();
        if let Some(only) = only {
            let wanted = Self::alias_closure(client, &entries, only, names).await;
            entries.retain(|entry| wanted.contains(&entry.jid.to_non_ad_string()));
            // The page above is the hundred most recently active chats, and a
            // narrowed load is about the chats somebody named — which is not
            // the same set. A chat that has fallen past that window would be
            // filtered down to nothing here, and a load with nothing in it
            // publishes nothing: the invalidation that asked for it would be
            // silently spent, leaving every front end on rows that changed.
            // Asked for by name instead, and only for what the page missed.
            let found: HashSet<String> = entries
                .iter()
                .map(|entry| entry.jid.to_non_ad_string())
                .collect();
            for jid in only.iter().filter(|jid| !found.contains(*jid)) {
                let Ok(parsed) = jid.parse::<Jid>() else {
                    continue;
                };
                match chat_store.chat(&parsed).await {
                    Ok(Some(entry)) => entries.push(entry),
                    // No row: the chat is live-only, or gone. Either way this
                    // load has nothing to say about it, which is what a
                    // narrowed load is allowed to be.
                    Ok(None) => {}
                    Err(e) => warn!(
                        "failed to look up {} for a scoped load: {e}",
                        observe_str(jid)
                    ),
                }
            }
        }
        // The other half of every row this load carries. A PN/LID pair is one
        // conversation and the collapse below is what makes its unread count
        // the pair's sum — but only over the rows it is given, and the window
        // above ends wherever the store's order puts it. Half a pair alone is
        // a chat with half the pair's unread count, and now that a front end
        // continues *past* this window rather than re-fetching it, nothing
        // else would go back for the other half. The cursor is already taken
        // from the raw boundary, so this cannot move where the list continues.
        let entries = if only.is_none() {
            Self::with_alias_rows(chat_store, client, names, entries).await
        } else {
            // A narrowed load has its own closure, which starts from the
            // chats somebody named rather than from a page.
            entries
        };
        let chats =
            Self::hydrate_entries(chat_store, client, names, entries, Self::attach_page).await?;
        Ok(LoadedHistory {
            chats,
            complete,
            next,
        })
    }

    /// Turn store rows into the chats a front end draws.
    ///
    /// The shared half of every read that produces chats: one page per chat
    /// in one read, the PN/LID collapse, reactions, sender names, the unread
    /// tail and the preview. `page_for` says how many messages each chat's
    /// page carries, because the two callers want different amounts — an
    /// attach carries what this side needs of a chat, a list page carries the
    /// row and nothing else.
    pub(super) async fn hydrate_entries(
        chat_store: &Arc<ChatStore>,
        client: &Arc<Client>,
        names: &NameBook,
        entries: Vec<ChatEntry>,
        page_for: impl Fn(&ChatEntry) -> i64,
    ) -> Result<Vec<oxidezap_core::Chat>, oxidezap_chat_store::ChatStoreError> {
        // Every chat's page in one read, before the loop that needs them: the
        // per-chat call is a permit, a blocking task and a transaction each,
        // and an attaching front end asks for a hundred of them at once.
        // Sized per chat by what this side needs of it — see `attach_page`.
        let mut pages = chat_store
            .pages(
                entries
                    .iter()
                    .map(|entry| (entry.jid.clone(), page_for(entry)))
                    .collect(),
            )
            .await?;
        let mut chats: Vec<oxidezap_core::Chat> = Vec::with_capacity(entries.len());
        // Updates whose stored ack says they were watched here. Gathered from
        // the rows as they are read and applied once at the end; see the call
        // to `apply_status_views` below for why it cannot be done in place.
        let mut status_views: HashSet<String> = HashSet::new();
        for entry in entries {
            // Same PN->LID mapping live events go through, or the restored
            // chat and the next live message split into two conversations.
            // A PN/LID pair of stored rows collapses into one chat: the most
            // recently active row (entries arrive in display order) keeps the
            // metadata, the older row's messages merge in.
            let identity = names.identity(client, &entry.jid).await;
            let (name, name_priority) = names
                .resolve(chat_store, &entry.jid, entry.name.as_deref(), &identity)
                .await;
            let jid_str = identity.canonical_jid.clone();
            if let Some(existing) = chats.iter_mut().find(|c| c.jid == jid_str) {
                let mut page = pages.remove(&entry.jid.to_string()).unwrap_or_default();
                page.reverse();
                if existing.is_status {
                    status_views.extend(watched_ids(&page));
                }
                let mut msgs: Vec<ChatMessage> =
                    page.into_iter().map(stored_to_chat_message).collect();
                Self::hydrate_reactions(chat_store, client, names, &entry.jid, &mut msgs).await;
                Self::hydrate_quoted_authors(client, names, &mut msgs).await;
                // Groups *and* the status broadcast: both carry rows written
                // by many people, and a hydrated row has no push name on it.
                if existing.is_group || existing.is_status {
                    Self::hydrate_sender_names(
                        chat_store,
                        client,
                        &mut msgs,
                        names,
                        existing.is_status,
                    )
                    .await;
                }
                // Each alias still needs its unread tail marked for receipts,
                // but PN/LID counters describe the same logical chat.
                mark_unread_tail(&mut msgs, entry.unread_count.max(0) as u32);
                merge_alias_history_messages(existing, msgs, entry.unread_count.max(0) as u32);
                // A page is assigned rather than added a row at a time, so
                // the naming `add_message` does per row has to be run over it.
                existing.name_quoted_authors();
                existing.manually_unread |= entry.unread_count < 0;
                existing.set_name_if_better(name, name_priority);
                continue;
            }
            // Store-originated: the HistoryLoaded prune may drop it when a
            // later complete load no longer returns it.
            let mut chat = oxidezap_core::Chat::from_store(jid_str.clone(), name, name_priority);
            chat.unread_count = entry.unread_count.max(0) as u32;
            // -1 = manually marked unread (WA Web convention); .max(0) above
            // must not silently eat the flag.
            chat.manually_unread = entry.unread_count < 0;
            chat.last_message_time = entry.last_message_at;

            let mut page = pages.remove(&entry.jid.to_string()).unwrap_or_default();
            page.reverse(); // store returns newest-first; the UI renders oldest-first
            if chat.is_status {
                status_views.extend(watched_ids(&page));
            }
            chat.messages = page.into_iter().map(stored_to_chat_message).collect();
            Self::hydrate_reactions(chat_store, client, names, &entry.jid, &mut chat.messages)
                .await;
            Self::hydrate_quoted_authors(client, names, &mut chat.messages).await;
            if chat.is_group || chat.is_status {
                let is_status = chat.is_status;
                Self::hydrate_sender_names(
                    chat_store,
                    client,
                    &mut chat.messages,
                    names,
                    is_status,
                )
                .await;
            }
            mark_unread_tail(&mut chat.messages, chat.unread_count);
            // After the sender names, because the best answer for "who wrote
            // the message this is replying to" is usually the reply's own
            // neighbour, and it has only just been named.
            chat.name_quoted_authors();
            chat.last_message =
                history_preview(entry.last_message_preview.clone(), chat.messages.last());
            chats.push(chat);
        }
        // The views come off the rows that were just read, not from a second
        // query: a watched update is one whose stored ack reached `Read`.
        // Applied in one pass at the end rather than inside the branches
        // above, because the alias merge re-marks rows unread from the chat
        // it merges into and would undo a fix applied before it.
        apply_status_views(&mut chats, &status_views);
        Ok(chats)
    }

    /// A page of chats, with each row's other half beside it.
    ///
    /// A PN/LID pair is one conversation and `hydrate_entries` is what
    /// collapses it — but only over the rows it is given, and a page boundary
    /// falls wherever the store's order puts it. Half a pair alone hydrates
    /// into a chat carrying half the pair's unread count, which the window
    /// merges over the whole one it already had.
    ///
    /// Both halves are pulled in, from whichever half the page holds, so the
    /// answer is the same collapsed chat either way: a page that lands after
    /// one that already carried this person repeats it rather than reducing
    /// it. Costs one read per row that has an alias the page does not.
    pub(super) async fn with_alias_rows(
        chat_store: &Arc<ChatStore>,
        client: &Arc<Client>,
        names: &NameBook,
        entries: Vec<ChatEntry>,
    ) -> Vec<ChatEntry> {
        let mut have: HashSet<String> = entries.iter().map(|e| e.jid.to_string()).collect();
        let mut wanted: Vec<Jid> = Vec::new();
        for entry in &entries {
            let identity = names.identity(client, &entry.jid).await;
            for alias in &identity.contact_jids {
                // `have` is what this page holds plus what has already been
                // asked for, so a pair whose halves are both on the page
                // costs nothing and neither half is asked for twice.
                if have.insert(alias.to_string()) {
                    wanted.push(alias.clone());
                }
            }
        }
        let mut entries = entries;
        if !wanted.is_empty() {
            // One read for the page, not one per alias: most people have a
            // single row, and finding that out a hundred times over is a
            // hundred permits and transactions spent on nothing.
            match chat_store.chats_by_jids(wanted).await {
                Ok(rows) => entries.extend(rows),
                Err(e) => log::warn!("could not read the aliases of a page of chats: {e}"),
            }
        }
        // Display order again, because that is what decides which half of a
        // pair keeps the metadata — the rows appended above are behind the
        // page's own until they are put back in it.
        entries.sort_by(|a, b| {
            let key = |e: &ChatEntry| {
                (
                    e.pinned_at.map(|t| t.timestamp_millis()),
                    e.last_message_at.map(|t| t.timestamp_millis()),
                )
            };
            key(b)
                .cmp(&key(a))
                .then_with(|| b.jid.to_string().cmp(&a.jid.to_string()))
        });
        entries
    }

    /// How many of one chat's newest messages the attach load carries.
    ///
    /// The unread tail, because those are the receipts a read owes and the
    /// second it is bounded by, with a floor that covers a same-second burst
    /// and the newest row the list previews from. The status broadcast is the
    /// exception: its feed *is* those rows — there is no conversation to open
    /// that would ask for more — so it keeps a whole page.
    pub(super) fn attach_page(entry: &ChatEntry) -> i64 {
        if is_status_broadcast(entry) {
            return Self::MESSAGE_PAGE;
        }
        i64::from(entry.unread_count.max(0)).clamp(Self::ATTACH_FLOOR, Self::ATTACH_CEILING)
    }

    /// Every storage key the invalidated chats are held under.
    ///
    /// A PN/LID pair collapses into one chat on load, and the collapse is what
    /// makes its unread counter the pair's sum: rebuilding one half alone
    /// would not be a smaller answer but a wrong one. The expansion runs off
    /// the entries the invalidated keys match, so it costs a mapping lookup
    /// per named chat rather than one per chat in the list.
    async fn alias_closure(
        client: &Arc<Client>,
        entries: &[ChatEntry],
        only: &HashSet<String>,
        names: &NameBook,
    ) -> HashSet<String> {
        let mut wanted = only.clone();
        for entry in entries {
            if !only.contains(&entry.jid.to_non_ad_string()) {
                continue;
            }
            let identity = names.identity(client, &entry.jid).await;
            wanted.insert(identity.canonical_jid.clone());
            wanted.extend(identity.contact_jids.iter().map(Jid::to_string));
        }
        wanted
    }

    /// Reactions live in their own table, so hydrated messages come out with
    /// an empty map; fold the stored rows back in. Per-message point lookups:
    /// the store exposes no per-chat batch query. Best-effort: one bad row
    /// must not abort the whole history load and blank the chat list.
    pub(super) async fn hydrate_reactions(
        chat_store: &Arc<ChatStore>,
        client: &Arc<Client>,
        names: &NameBook,
        chat_jid: &Jid,
        msgs: &mut [ChatMessage],
    ) {
        // One query for the page. A message with no reactions is the common
        // case by a wide margin, and asking per message spent a pooled read on
        // each of them.
        let ids: Vec<String> = msgs.iter().map(|msg| msg.id.clone()).collect();
        let mut by_message = match chat_store.reactions_for(chat_jid, ids).await {
            Ok(found) => found,
            Err(e) => {
                warn!(
                    "failed to hydrate reactions for {}: {e}",
                    observe_str(&chat_jid.to_string())
                );
                return;
            }
        };
        for msg in msgs.iter_mut() {
            let Some(entries) = by_message.remove(&msg.id) else {
                continue;
            };
            // The store keeps one row per sender, and the live path publishes
            // reactors under their canonical JID — so a row stored under one
            // alias has to be read back under the same name, or a later
            // replacement or removal cannot find it and the two aliases stand
            // as two people. Coalesced here as well as renamed: two rows *are*
            // two rows in the table, and the answer is the same one the live
            // path gives, which is that the newest wins. Rows arrive oldest
            // first, so the last write is it.
            //
            // A linear scan rather than a map: a message has a handful of
            // reactors, and keeping the order they were stored in is what
            // makes a reloaded row draw them the way the live one did.
            let mut latest: Vec<(String, String)> = Vec::new();
            for entry in entries {
                let who = names.identity(client, &entry.sender_jid).await;
                match latest.iter_mut().find(|(jid, _)| *jid == who.canonical_jid) {
                    Some((_, emoji)) => *emoji = entry.emoji,
                    None => latest.push((who.canonical_jid.clone(), entry.emoji)),
                }
            }
            // Through `add_reaction` rather than into the map, because the
            // bounds on a message's reactions live there: writing the rows
            // straight in restored every stored reactor, so a message the
            // live path had capped came back over the cap after a reload —
            // and drew a different set from the copy beside it.
            for (sender, emoji) in latest {
                msg.add_reaction(emoji, sender);
            }
        }
    }

    /// File a quote's author under the identity their own bubbles are filed
    /// under, and name them from the same book those bubbles are named from.
    ///
    /// Every other sender field on a message goes through
    /// `identity.canonical_jid`; the one on a quote came straight off the
    /// envelope, which is a phone number where the chat is keyed by a LID and
    /// carries the sending device's suffix besides. `Chat::quoted_author`
    /// looks a participant up by exact string, so the bar above a reply read
    /// "Unknown contact" — or a bare number — over bubbles from the same
    /// person, named from the address book, an inch above it.
    ///
    /// Canonical is not enough on its own: the participant map only holds
    /// whoever has a row on the loaded page, so a reply to a message that
    /// scrolled past — or was never loaded — still had nobody to be named by,
    /// while the address book knew them all along. That book is what names a
    /// bubble, so it is what names the bar above one.
    ///
    /// Whoever this account is stays unnamed here on purpose:
    /// `Chat::quoted_author` calls them "You", which is the better answer and
    /// the one a reader expects over their own message.
    pub(super) async fn hydrate_quoted_authors(
        client: &Arc<Client>,
        names: &NameBook,
        msgs: &mut [ChatMessage],
    ) {
        let me = own_jids(client);
        for msg in msgs.iter_mut() {
            let Some(quoted) = msg.quoted.as_mut() else {
                continue;
            };
            let Ok(jid) = quoted.sender.parse::<Jid>() else {
                continue;
            };
            let identity = names.identity(client, &jid).await;
            quoted.sender.clone_from(&identity.canonical_jid);
            if quoted.sender_name.is_empty()
                && !me.iter().any(|own| *own == identity.canonical_jid)
                && let Some(name) = names.known(client, &jid, None).await
            {
                quoted.sender_name = name;
            }
        }
    }

    /// Group bubbles label their sender, but a hydrated row carries no push
    /// name; the book answers from the same order the live path uses, so a
    /// reloaded bubble and the one that arrived a moment ago agree.
    ///
    /// A group page names the same handful of people over and over and the
    /// book memoizes per JID, so a page costs one lookup per unique sender
    /// rather than one per row.
    pub(super) async fn hydrate_sender_names(
        chat_store: &Arc<ChatStore>,
        client: &Arc<Client>,
        msgs: &mut [ChatMessage],
        names: &NameBook,
        is_status: bool,
    ) {
        for msg in msgs.iter_mut() {
            if msg.is_from_me || msg.sender_name.is_some() {
                continue;
            }
            let Ok(jid) = msg.sender.parse::<Jid>() else {
                continue;
            };
            let identity = names.identity(client, &jid).await;
            // One person, one row in the feed. The status broadcast is
            // grouped by sender, and the same contact reaches it under a
            // phone number on some updates and their LID on others — which
            // split their ring, their unseen count and their playback run in
            // two. Chat identities are canonicalized on the way in; these had
            // been left as they arrived.
            if is_status {
                msg.sender.clone_from(&identity.canonical_jid);
            }
            // The same answer the live path gives, and for the same reason a
            // number is not one: this field only ever gains a value, because
            // `Chat::update_participant` fills blanks. A row stamped with a
            // phone number could never be renamed by the push name that
            // arrives a second later, and the same person would read as a
            // number on their reloaded bubbles and by name on their new ones.
            // Drawing a number where nothing is known is the *renderer's* job.
            msg.sender_name = names.named(chat_store, &jid, None, &identity).await;
        }
    }
}

/// Both addresses this account answers to, as they are written on a row.
///
/// A device that has paired but never synced has neither, which is the same
/// as not recognising itself — and a message of one's own is labelled from
/// its own `is_from_me` rather than from this, so nothing depends on it.
pub(super) fn own_jids(client: &Arc<Client>) -> Vec<String> {
    let device = client.persistence_manager().get_device_snapshot();
    [device.pn.as_ref(), device.lid.as_ref()]
        .into_iter()
        .flatten()
        .map(|jid| jid.to_non_ad_string())
        .collect()
}

/// The one chat nobody opens as a conversation.
fn is_status_broadcast(entry: &ChatEntry) -> bool {
    entry.jid.to_non_ad_string() == oxidezap_core::STATUS_BROADCAST_JID
}

pub(super) fn merge_alias_history_messages(
    chat: &mut Chat,
    mut messages: Vec<ChatMessage>,
    alias_unread: u32,
) {
    // Alias rows may be disjoint or repeated; only loaded message IDs prove
    // that two unread counters overlap.
    let existing_unread_ids: HashSet<String> = chat
        .messages
        .iter()
        .filter(|message| !message.is_from_me && !message.is_read)
        .map(|message| message.id.clone())
        .collect();
    let duplicate_unread = messages
        .iter()
        .filter(|message| {
            !message.is_from_me && !message.is_read && existing_unread_ids.contains(&message.id)
        })
        .count() as u32;

    for message in &mut messages {
        if !message.is_from_me && existing_unread_ids.contains(&message.id) {
            message.is_read = false;
        }
    }
    for message in messages {
        chat.insert_history_message(message);
    }

    let visible_unread = chat
        .messages
        .iter()
        .filter(|message| !message.is_from_me && !message.is_read)
        .count() as u32;
    chat.unread_count = chat
        .unread_count
        .saturating_add(alias_unread)
        .saturating_sub(duplicate_unread)
        .max(visible_unread);
}

/// The updates in `page` whose stored ack says they were watched here.
///
/// `Read` on an incoming row means exactly that and nothing else: the column
/// is written once at insert as `Delivered`, peer receipts only advance our
/// own messages, and a redelivery refreshes content without touching it. The
/// same field WhatsApp Web moves to `ACK.READ` when a status is viewed.
fn watched_ids(page: &[oxidezap_chat_store::StoredMessage]) -> impl Iterator<Item = String> + '_ {
    page.iter()
        .filter(|stored| {
            !stored.from_me
                && matches!(
                    stored.status,
                    oxidezap_chat_store::MessageStatus::Read
                        | oxidezap_chat_store::MessageStatus::Played
                )
        })
        .map(|stored| stored.id.clone())
}

/// Mark the watched updates read, in the broadcast and nowhere else.
///
/// Our own updates are left alone: they are never unseen to begin with, and a
/// row from us carries the peer-read ticks in `is_read`, which a local view has
/// no business setting.
pub(super) fn apply_status_views(chats: &mut [oxidezap_core::Chat], watched: &HashSet<String>) {
    if watched.is_empty() {
        return;
    }
    for chat in chats.iter_mut().filter(|chat| chat.is_status) {
        for message in &mut chat.messages {
            if !message.is_from_me && watched.contains(&message.id) {
                message.is_read = true;
            }
        }
    }
}

/// The chat-list preview for a hydrated chat.
///
/// The store's `last_message_preview` is the newest message's TEXT, and plenty
/// of messages have none: a photo or a voice note without a caption, a revoked
/// message whose content was tombstoned. The bubble still has a label — the
/// same one the live path puts in the list — so it answers where the column
/// cannot, and a chat that plainly has messages stops rendering as "No
/// messages".
fn history_preview(stored: Option<String>, newest: Option<&ChatMessage>) -> Option<String> {
    stored.or_else(|| newest.map(ChatMessage::preview_text))
}
