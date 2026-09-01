//! What the daemon remembers about reads, and nothing else.
//!
//! Domain bookkeeping over the same event stream that feeds the hub: which
//! messages a chat has been handed, where a read may stop, which receipts are
//! owed, and which badge a store reload is not allowed to put back. It holds
//! no client and no state hub — what it is asked about arrives as a parameter.

use std::collections::{HashMap, HashSet, VecDeque};

use oxidezap_core::{Chat, ChatMessage, UiEvent};
use oxidezap_session::ReadBoundary;

/// The "newest message second" of a chat with no message at all.
///
/// A real timestamp is always greater, so a chat that gains its first message
/// always counts as having moved past a read issued while it was empty.
const NOTHING_BEHIND_IT: i64 = i64::MIN;

/// How long a read the store has not confirmed may keep a badge down.
///
/// The override exists to cover the reload that was already in flight, which
/// lands within the store reloader's debounce. Past that, a store that still
/// disagrees is not a race — the action failed, and the session reports that
/// nowhere the daemon can see. Letting the badge come back is then the honest
/// answer: the chat really is unread, and a badge suppressed forever would be
/// a lie the user cannot correct.
const READ_OVERRIDE_GRACE_MS: i64 = 30_000;

/// A read this daemon issued and the store has not confirmed yet.
#[derive(Debug)]
pub(super) struct ReadRecord {
    /// The second the read action covered.
    secs: i64,
    /// The messages at that second it named. A read clears whole seconds, but
    /// a message arriving *afterwards* can land in the same one, and that is
    /// a genuinely unread message the read never covered — the ids are how it
    /// is told apart from the ones that were.
    ids: HashSet<String>,
    /// When this stops applying. See [`READ_OVERRIDE_GRACE_MS`].
    expires_at_ms: i64,
}

impl ReadRecord {
    pub(super) fn through(secs: i64, boundary: &[(String, bool, Option<String>)]) -> Self {
        Self {
            secs,
            ids: boundary.iter().map(|(id, ..)| id.clone()).collect(),
            expires_at_ms: wacore::time::now_millis().saturating_add(READ_OVERRIDE_GRACE_MS),
        }
    }

    /// A read of a chat that had no message at all.
    pub(super) fn nothing_behind_it() -> Self {
        Self::through(NOTHING_BEHIND_IT, &[])
    }

    /// Whether this read already covered `message`.
    fn covers(&self, message: &ChatMessage) -> bool {
        let secs = message.timestamp.timestamp();
        secs < self.secs || (secs == self.secs && self.ids.contains(&message.id))
    }

    /// Whether `message` is a reason to stop trusting this read.
    ///
    /// Only an incoming message nobody has read: a receipt is owed for those
    /// and for nothing else. Our own message echoed back is the one most
    /// likely to land inside the very window this override covers — the user
    /// opens the chat, the read goes out, they answer — and taking it as
    /// proof the read is stale ends the override with the reload it was
    /// waiting for still in flight, so the badge comes straight back.
    fn undermined_by(&self, message: &ChatMessage) -> bool {
        !message.is_from_me && !message.is_read && !self.covers(message)
    }

    fn expired(&self) -> bool {
        wacore::time::now_millis() > self.expires_at_ms
    }
}

/// Most unread messages the daemon will remember per chat.
///
/// Receipts are a courtesy to the sender, not correctness: a chat with more
/// than this outstanding has been unattended for a very long time, and
/// remembering every id for it would let one abandoned conversation grow the
/// daemon without bound. The oldest are dropped first, so the ones a user is
/// most likely to care about survive.
const MAX_TRACKED_UNREAD: usize = 512;

/// What `MarkRead` needs and a [`oxidezap_ipc::ChatSummary`] cannot carry.
///
/// A summary is a badge and a preview. Turning the sender's ticks blue needs
/// message ids, and persisting the read across devices needs the timestamp
/// boundary — including every sibling at the same second, or a message the
/// boundary excluded re-badges the chat on the next hydration. The daemon
/// deliberately holds no messages, so it keeps exactly this much and no more.
#[derive(Default)]
struct ChatReads {
    /// Newest message timestamp seen, in whole seconds.
    newest_secs: i64,
    /// Every message at `newest_secs`, shaped as `mark_chat_read` wants them.
    boundary: Vec<(String, bool, Option<String>)>,
    /// Incoming messages still unread, shaped as `send_read_receipts` wants
    /// them.
    unread: VecDeque<(String, String)>,
    /// Every incoming message this chat has been handed, receipt owed or not.
    ///
    /// Separate from `unread` because that one is drained when the receipts
    /// go out: after a read, it remembers nothing, and a redelivery of a
    /// message already read would come back as a first sighting and put the
    /// badge up over it. Ids only, and bounded the same way.
    seen: VecDeque<String>,
}

impl ChatReads {
    /// Drop what this chat remembers about `ids`, so a fresh answer about
    /// them can be folded in.
    ///
    /// Both queues, because `seen` is what makes `observe` skip a message it
    /// recognises: left behind, a message the store still reports unread
    /// would never be queued for its receipt again.
    fn forget<'a>(&mut self, ids: impl IntoIterator<Item = &'a str>) {
        let ids: std::collections::HashSet<&str> = ids.into_iter().collect();
        self.unread.retain(|(id, _)| !ids.contains(id.as_str()));
        self.seen.retain(|id| !ids.contains(id.as_str()));
    }

    /// Fold one message in, answering whether it is an incoming message this
    /// chat had not seen before.
    ///
    /// The answer is what the badge counts. The two bookkeepings used to
    /// disagree: the same `MessageReceived` delivered twice (offline
    /// catch-up, a retry) added 2 to the badge while this side recognised
    /// the duplicate and owed one receipt, and nothing reconciled them until
    /// a complete store reload.
    fn observe(&mut self, message: &ChatMessage) -> bool {
        let secs = message.timestamp.timestamp();
        // A backfill older than what we hold says nothing about the boundary.
        if secs > self.newest_secs {
            self.newest_secs = secs;
            self.boundary.clear();
        }
        if secs == self.newest_secs && !self.boundary.iter().any(|(id, ..)| *id == message.id) {
            self.boundary.push((
                message.id.clone(),
                message.is_from_me,
                (!message.is_from_me).then(|| message.sender.clone()),
            ));
        }

        if message.is_from_me || self.seen.iter().any(|id| *id == message.id) {
            return false;
        }
        self.seen.push_back(message.id.clone());
        if self.seen.len() > MAX_TRACKED_UNREAD {
            self.seen.pop_front();
        }
        // Seen either way: a message the store already reports as read is one
        // this chat has been handed, and owes no receipt for.
        if message.is_read {
            return false;
        }
        self.unread
            .push_back((message.id.clone(), message.sender.clone()));
        if self.unread.len() > MAX_TRACKED_UNREAD {
            self.unread.pop_front();
        }
        true
    }

    fn boundary(&self) -> Option<ReadBoundary> {
        (!self.boundary.is_empty()).then(|| (self.newest_secs, self.boundary.clone()))
    }
}

/// Per-chat read state, fed by the same event stream that feeds the hub.
#[derive(Default)]
pub(super) struct ReadTracker {
    chats: HashMap<String, ChatReads>,
    /// Chats this daemon has marked read and the store has not confirmed.
    ///
    /// Separate from `chats` because a store reload rebuilds that map wholesale
    /// while this has to survive exactly such a reload — it exists to outlive
    /// the one that is already in flight.
    read_through: HashMap<String, ReadRecord>,
}

impl ReadTracker {
    /// Fold one session event in, answering whether it carried an incoming
    /// message the chat had not seen before. That is what the badge counts:
    /// see [`ChatReads::observe`].
    pub(super) fn observe(&mut self, event: &UiEvent) -> bool {
        match event {
            UiEvent::MessageReceived {
                chat_jid, message, ..
            } => {
                let first_sighting = self
                    .chats
                    .entry(chat_jid.clone())
                    .or_default()
                    .observe(message);
                // A message the read never covered ends the override here
                // too, so it cannot suppress a badge this message legitimately
                // raised. By coverage rather than by time: an arrival landing
                // in the same second as the boundary is still one the read
                // did not name.
                if self
                    .read_through
                    .get(chat_jid)
                    .is_some_and(|read| read.undermined_by(message))
                {
                    self.read_through.remove(chat_jid);
                }
                first_sighting
            }
            UiEvent::HistoryLoaded { chats, .. } => {
                for chat in chats {
                    let reads = self.chats.entry(chat.jid.clone()).or_default();
                    // The boundary is only the store's answer when the load
                    // reaches it. The same rule `observe` holds one message at
                    // a time: a page older than what this side holds says
                    // nothing about where the chat ends, and rebuilding from
                    // one let the boundary recede — the window then named a
                    // message it had just drawn in a read the daemon refused,
                    // which is a badge that clears locally, sends no receipt
                    // and comes back on the next hydration. A chat the store
                    // reports with nothing in it is the one case where
                    // receding is the answer.
                    let newest = chat
                        .messages
                        .iter()
                        .map(|message| message.timestamp.timestamp())
                        .max();
                    if newest.is_none_or(|newest| newest >= reads.newest_secs) {
                        *reads = ChatReads::default();
                    } else {
                        // An older page answers for the rows in it and for
                        // nothing else. Clearing the whole queue here dropped
                        // the receipt owed for a live message the page does
                        // not carry: the boundary still admitted a read
                        // naming it, and no receipt went out for it.
                        reads.forget(chat.messages.iter().map(|message| message.id.as_str()));
                    }
                    for message in &chat.messages {
                        reads.observe(message);
                    }
                }
                false
            }
            _ => false,
        }
    }

    /// Fold in one message of a page this daemon served.
    ///
    /// The same bookkeeping an event does, for history that reached a front
    /// end without passing through the event stream. A page is what a window
    /// is about to read, and a read is bounded by what this side has seen: a
    /// window naming a message from a page nobody told the daemon about is
    /// refused, and the badge comes back on the next hydration.
    pub(super) fn observe_message(&mut self, jid: &str, message: &ChatMessage) {
        self.chats
            .entry(jid.to_string())
            .or_default()
            .observe(message);
    }

    /// Where a read action for `jid` must stop, if the daemon knows.
    pub(super) fn boundary(&self, jid: &str) -> Option<ReadBoundary> {
        self.chats.get(jid).and_then(ChatReads::boundary)
    }

    /// Take the receipts this chat owes, leaving the boundary behind.
    ///
    /// The boundary describes where the chat ends, which the next read still
    /// has to know even though these receipts have gone out.
    pub(super) fn take_receipts(&mut self, jid: &str) -> Vec<(String, String)> {
        self.chats
            .get_mut(jid)
            .map(|reads| reads.unread.drain(..).collect())
            .unwrap_or_default()
    }

    /// Everything this account taught us, gone with it.
    pub(super) fn forget_all(&mut self) {
        self.chats.clear();
        self.read_through.clear();
    }

    /// Remember a read the store has not confirmed yet.
    pub(super) fn record_read(&mut self, jid: &str, read: ReadRecord) {
        self.read_through.insert(jid.to_string(), read);
    }

    /// Whether a store reload's unread count for `chat` is about messages this
    /// daemon has already read.
    ///
    /// Spends the override every way it can stop being true, so it papers over
    /// exactly the window it was meant for and no longer:
    ///
    /// * the store agrees, so there is nothing left to paper over — after
    ///   which a chat marked unread on another device comes through untouched;
    /// * the reload names an unread message the read never covered, including
    ///   one that landed in the boundary's own second;
    /// * the chat's newest message is past the read, which catches the same
    ///   thing for a reload that carries counts without hydrated messages;
    /// * the grace ran out, which is what a read that simply failed looks
    ///   like from here.
    pub(super) fn overrides_unread(&mut self, chat: &Chat, store_agrees: bool) -> bool {
        let Some(read) = self.read_through.get(&chat.jid) else {
            return false;
        };

        // The newest message a receipt could be owed for. `last_message_time`
        // is whatever arrived last, ours included, and answers only where the
        // reload carried counts without rows to look at.
        let newest_secs = if chat.messages.is_empty() {
            chat.last_message_time
                .map_or(NOTHING_BEHIND_IT, |t| t.timestamp())
        } else {
            chat.messages
                .iter()
                .rev()
                .find(|m| !m.is_from_me)
                .map_or(NOTHING_BEHIND_IT, |m| m.timestamp.timestamp())
        };
        let spent = store_agrees
            || read.expired()
            || newest_secs > read.secs
            || chat.messages.iter().any(|m| read.undermined_by(m));

        if spent {
            self.read_through.remove(&chat.jid);
            return false;
        }
        true
    }

    pub(super) fn forget(&mut self, jid: &str) {
        self.chats.remove(jid);
        self.read_through.remove(jid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Exercised through the bridge, which is what feeds it: a read is bounded
    // by what the daemon has observed, so the events that teach it are the
    // fixture. See `crate::session_bridge::tests`.
    use crate::session_bridge::tests::{bridge, loaded, message, received, stored_chat};

    /// What `mark_read` would record after issuing a read of `secs` covering
    /// `ids`.
    fn read_through(secs: i64, ids: &[&str]) -> ReadRecord {
        let boundary: Vec<(String, bool, Option<String>)> = ids
            .iter()
            .map(|id| ((*id).to_string(), false, None))
            .collect();
        ReadRecord::through(secs, &boundary)
    }

    /// The same message delivered twice (offline catch-up, a retry) added 2
    /// to the badge, while the read tracker recognised the duplicate and
    /// owed one receipt. Two counts of one thing, disagreeing until a
    /// complete store reload happened to settle it.
    #[test]
    fn a_redelivered_message_is_counted_once() {
        let mut bridge = bridge();
        let arrival = || {
            received(
                "1@s.whatsapp.net",
                message("m1", "1@s.whatsapp.net", 10, false, false),
                None,
            )
        };
        bridge.observe(arrival());
        bridge.observe(arrival());

        assert_eq!(bridge.hub.chat("1@s.whatsapp.net").unwrap().unread, 1);
        assert_eq!(
            bridge.reads().take_receipts("1@s.whatsapp.net").len(),
            1,
            "and the badge agrees with the receipts this side owes"
        );
    }

    /// The receipts a chat owes are drained when they go out, so they cannot
    /// also be the memory of what it has seen: a redelivery after the read
    /// found nothing to match, counted as a first sighting, and put the badge
    /// back up over a message the user had already read.
    #[test]
    fn a_redelivery_after_the_read_does_not_raise_the_badge_again() {
        let mut bridge = bridge();
        let arrival = || {
            received(
                "1@s.whatsapp.net",
                message("m1", "1@s.whatsapp.net", 10, false, false),
                None,
            )
        };
        bridge.observe(arrival());
        // What marking the chat read does to this side.
        assert_eq!(bridge.reads().take_receipts("1@s.whatsapp.net").len(), 1);

        bridge.observe(arrival());

        assert_eq!(
            bridge.hub.chat("1@s.whatsapp.net").unwrap().unread,
            1,
            "the badge is not raised a second time by the same message"
        );
        assert!(
            bridge.reads().take_receipts("1@s.whatsapp.net").is_empty(),
            "and no second receipt is owed for it"
        );
    }

    /// Receipts need message ids the summary does not carry, and the bounded
    /// action needs every sibling at the newest second or one of them
    /// re-badges the chat on the next hydration.
    #[test]
    fn read_state_collects_the_boundary_and_the_receipts_it_owes() {
        let mut bridge = bridge();
        for m in [
            message("older", "1@s.whatsapp.net", 10, false, false),
            message("a", "1@s.whatsapp.net", 20, false, false),
            // Same second as `a`: a boundary that excluded it would leave it
            // unread and let it re-badge the chat.
            message("b", "1@s.whatsapp.net", 20, false, false),
            // Ours, and already-read ones, owe no receipt.
            message("mine", "Me", 20, true, false),
        ] {
            bridge.observe(received("1@s.whatsapp.net", m, None));
        }

        let (boundary, read) = bridge
            // The newest message is `mine`, so that is what a client's preview
            // names.
            .read_plan("1@s.whatsapp.net", Some("mine"))
            .expect("a client that is up to date may read");
        let (secs, ids) = boundary.expect("a chat with messages has a boundary");
        assert_eq!((secs, read.secs), (20, 20));
        let mut at_boundary: Vec<&str> = ids.iter().map(|(id, ..)| id.as_str()).collect();
        at_boundary.sort_unstable();
        assert_eq!(at_boundary, ["a", "b", "mine"]);

        let mut owed: Vec<String> = bridge
            .reads()
            .take_receipts("1@s.whatsapp.net")
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        owed.sort_unstable();
        assert_eq!(owed, ["a", "b", "older"]);

        assert!(
            bridge.reads().take_receipts("1@s.whatsapp.net").is_empty(),
            "a receipt is owed once, not every time"
        );
        assert!(
            bridge.reads().boundary("1@s.whatsapp.net").is_some(),
            "the boundary outlives the receipts: the next read still needs it"
        );
    }

    /// A read is irreversible. A client acting on a chat that has moved on
    /// since it last looked would consume an arrival nobody ever saw, and
    /// `MarkRead` carries only a JID unless the client says what it saw.
    #[test]
    fn a_read_from_a_client_that_has_fallen_behind_is_refused() {
        let mut bridge = bridge();
        bridge.observe(received(
            "1@s.whatsapp.net",
            message("seen", "1@s.whatsapp.net", 10, false, false),
            None,
        ));
        // The client rendered this much and asked to mark it read...
        // ...but another message landed first.
        bridge.observe(received(
            "1@s.whatsapp.net",
            message("unseen", "1@s.whatsapp.net", 20, false, false),
            None,
        ));

        let refusal = bridge
            .read_plan("1@s.whatsapp.net", Some("seen"))
            .expect_err("must not mark read what nobody has seen");
        assert!(refusal.contains("does not cover"), "{refusal}");

        // Caught up, and it goes through.
        assert!(bridge.read_plan("1@s.whatsapp.net", Some("unseen")).is_ok());
    }

    /// The two sides of a burst do not agree on which of it came last, and
    /// they are both right.
    ///
    /// WhatsApp stamps to the second, so a ping and its pong are one
    /// timestamp. The store returns them in arrival order and a front end
    /// sorts them by `(timestamp, id)`, so `messages.last()` names a different
    /// message on each side whenever id order and arrival order disagree.
    /// Requiring the request to echo *the daemon's* last message therefore
    /// refused every read of such a chat, for good: the receipt never went
    /// out, the read was never persisted, and the badge came back on the next
    /// hydration. The advice in the refusal could not even be followed —
    /// asking again produced the same id.
    ///
    /// A read clears whole seconds, so naming either sibling has exactly the
    /// same effect. Both are honest claims to have seen the burst.
    #[test]
    fn either_half_of_a_one_second_burst_is_a_read_of_the_burst() {
        let mut bridge = bridge();
        for id in ["pong", "ping"] {
            bridge.observe(received(
                "1@s.whatsapp.net",
                message(id, "1@s.whatsapp.net", 20, false, false),
                None,
            ));
        }
        // Both are at the same second, so the preview keeps whichever the
        // tie-break puts last — and a front end sorting its own messages can
        // land on either. Neither side is behind the other.
        let daemon_newest = bridge
            .hub
            .chat("1@s.whatsapp.net")
            .unwrap()
            .last_message
            .and_then(|m| m.id);
        assert_eq!(daemon_newest.as_deref(), Some("pong"));

        assert!(
            bridge.read_plan("1@s.whatsapp.net", Some("pong")).is_ok(),
            "the id a front end would echo has to be accepted"
        );
        assert!(bridge.read_plan("1@s.whatsapp.net", Some("ping")).is_ok());
    }

    /// The daemon's hydrated messages and the store's preview columns are
    /// different rows and can drift. A boundary that does not contain the
    /// message the client is looking at would clear a second the client has
    /// no view of at all.
    #[test]
    fn a_boundary_that_does_not_cover_the_preview_is_refused() {
        let mut bridge = bridge();
        // Preview says one thing; the hydrated tail says another.
        let mut chat = stored_chat(
            "1@s.whatsapp.net",
            2,
            vec![message("hydrated", "1@s.whatsapp.net", 10, false, false)],
        );
        chat.last_message = Some("newer".into());
        chat.last_message_time = Some(chrono::DateTime::from_timestamp(20, 0).unwrap());
        bridge.observe(loaded(vec![chat]));

        // The preview still names the hydrated message, so that is what a
        // client echoes; the guard is that the boundary must contain it.
        let plan = bridge.read_plan("1@s.whatsapp.net", Some("hydrated"));
        assert!(plan.is_ok(), "the boundary does contain it: {plan:?}");

        // Now the same chat with a preview naming nothing the daemon holds.
        let mut chat = stored_chat("1@s.whatsapp.net", 2, Vec::new());
        chat.last_message = Some("newer".into());
        chat.last_message_time = Some(chrono::DateTime::from_timestamp(20, 0).unwrap());
        bridge.observe(loaded(vec![chat]));
        let refusal = bridge
            .read_plan("1@s.whatsapp.net", None)
            .expect_err("nothing ties the preview to a message");
        assert!(refusal.contains("no message boundary"), "{refusal}");
    }

    /// An unbounded read action clears a chat by its own timestamp. Issuing
    /// one for a chat the daemon knows holds messages it has not seen would
    /// consume arrivals the requester never laid eyes on.
    #[test]
    fn a_chat_with_unseen_messages_will_not_be_marked_read_unbounded() {
        let mut bridge = bridge();
        // A preview with no message behind it: hydrated summary, messages not
        // loaded. Exactly the case the daemon cannot bound.
        let mut chat = stored_chat("1@s.whatsapp.net", 4, Vec::new());
        chat.last_message = Some("hi".into());
        chat.last_message_time = Some(chrono::DateTime::from_timestamp(10, 0).unwrap());
        bridge.observe(loaded(vec![chat]));

        let refusal = bridge
            .read_plan("1@s.whatsapp.net", None)
            .expect_err("must not run unbounded");
        assert!(refusal.contains("no message boundary"), "{refusal}");
    }

    /// The other side of it: a chat with nothing behind it has nothing to
    /// bound, and refusing that would make a badge-only chat impossible to
    /// clear.
    #[test]
    fn a_chat_with_nothing_behind_it_needs_no_boundary() {
        let mut bridge = bridge();
        let mut chat = stored_chat("1@s.whatsapp.net", 0, Vec::new());
        chat.manually_unread = true;
        bridge.observe(loaded(vec![chat]));

        let (boundary, read) = bridge
            .read_plan("1@s.whatsapp.net", None)
            .expect("a chat with nothing behind it needs no bound");
        assert!(boundary.is_none());
        assert_eq!(read.secs, NOTHING_BEHIND_IT);
    }

    /// A chat the daemon has never held at all is not something it can act
    /// on, bounded or otherwise.
    #[test]
    fn an_unknown_chat_is_refused_outright() {
        let bridge = bridge();
        assert!(
            bridge
                .read_plan("nobody@s.whatsapp.net", None)
                .unwrap_err()
                .contains("no such chat")
        );
    }

    /// The race the override exists for: the store reload was scheduled by the
    /// very message that raised the badge, so it still reports the old count
    /// when it lands just after an accepted read. Republishing it puts the
    /// badge straight back, moments after the user cleared it.
    #[test]
    fn a_reload_in_flight_cannot_undo_a_read_that_was_just_accepted() {
        let mut bridge = bridge();
        let incoming = message("m1", "1@s.whatsapp.net", 10, false, false);
        bridge.observe(received("1@s.whatsapp.net", incoming.clone(), None));
        assert_eq!(bridge.hub.chat("1@s.whatsapp.net").unwrap().unread, 1);

        // What `mark_read` records once it has issued the action.
        bridge
            .reads()
            .record_read("1@s.whatsapp.net", read_through(10, &["m1"]));

        // The reload the store already had queued, still carrying the count.
        bridge.observe(loaded(vec![stored_chat(
            "1@s.whatsapp.net",
            1,
            vec![incoming],
        )]));
        assert_eq!(
            bridge.hub.chat("1@s.whatsapp.net").unwrap().unread,
            0,
            "the badge stays down"
        );
    }

    /// Answering is the likeliest thing to happen inside the window the
    /// override covers — open the chat, the read goes out, type a reply — and
    /// the echo of that reply used to end the override: the reload still in
    /// flight then republished the old count and the badge came straight
    /// back, in the exact race the mechanism exists for.
    #[test]
    fn answering_does_not_bring_the_badge_back() {
        let mut bridge = bridge();
        let incoming = message("m1", "1@s.whatsapp.net", 10, false, false);
        bridge.observe(received("1@s.whatsapp.net", incoming.clone(), None));
        bridge
            .reads()
            .record_read("1@s.whatsapp.net", read_through(10, &["m1"]));

        let reply = message("m2", "me@s.whatsapp.net", 20, true, false);
        bridge.observe(received("1@s.whatsapp.net", reply.clone(), None));
        bridge.observe(loaded(vec![stored_chat(
            "1@s.whatsapp.net",
            1,
            vec![incoming, reply],
        )]));

        assert_eq!(
            bridge.hub.chat("1@s.whatsapp.net").unwrap().unread,
            0,
            "the badge stays down"
        );
    }

    /// The override is spent, not permanent: a message arriving after the read
    /// raises the badge again, and a later reload reports it untouched.
    #[test]
    fn a_message_after_the_read_badges_the_chat_again() {
        let mut bridge = bridge();
        let first = message("m1", "1@s.whatsapp.net", 10, false, false);
        bridge.observe(received("1@s.whatsapp.net", first.clone(), None));
        bridge
            .reads()
            .record_read("1@s.whatsapp.net", read_through(10, &["m1"]));

        let second = message("m2", "1@s.whatsapp.net", 20, false, false);
        bridge.observe(received("1@s.whatsapp.net", second.clone(), None));
        bridge.observe(loaded(vec![stored_chat(
            "1@s.whatsapp.net",
            1,
            vec![first, second],
        )]));

        assert_eq!(
            bridge.hub.chat("1@s.whatsapp.net").unwrap().unread,
            1,
            "a message the user has not seen still badges"
        );
    }

    /// A message can land in the very second the read covered, and it is not
    /// one the read named. Comparing whole seconds would call it covered and
    /// suppress a badge the user should see.
    #[test]
    fn a_same_second_arrival_after_the_read_still_badges() {
        let mut bridge = bridge();
        let read_msg = message("m1", "1@s.whatsapp.net", 20, false, false);
        bridge.observe(received("1@s.whatsapp.net", read_msg.clone(), None));
        bridge
            .reads()
            .record_read("1@s.whatsapp.net", read_through(20, &["m1"]));

        // Same second, different message: the action named `m1`, not this.
        let sibling = message("m2", "1@s.whatsapp.net", 20, false, false);
        bridge.observe(received("1@s.whatsapp.net", sibling.clone(), None));
        bridge.observe(loaded(vec![stored_chat(
            "1@s.whatsapp.net",
            1,
            vec![read_msg, sibling],
        )]));

        assert_eq!(
            bridge.hub.chat("1@s.whatsapp.net").unwrap().unread,
            1,
            "a sibling the read never covered is genuinely unread"
        );
    }

    /// A read that simply failed reports nothing the daemon can see, so the
    /// override cannot wait for a confirmation that is never coming. Past its
    /// grace the store wins and the badge returns, which is the truth.
    #[test]
    fn an_unconfirmed_read_stops_suppressing_the_badge_once_its_grace_is_up() {
        let mut bridge = bridge();
        let only = message("m1", "1@s.whatsapp.net", 20, false, false);
        bridge.observe(received("1@s.whatsapp.net", only.clone(), None));

        let mut stale = read_through(20, &["m1"]);
        stale.expires_at_ms = wacore::time::now_millis() - 1;
        bridge.reads().record_read("1@s.whatsapp.net", stale);

        bridge.observe(loaded(vec![stored_chat("1@s.whatsapp.net", 1, vec![only])]));
        assert_eq!(
            bridge.hub.chat("1@s.whatsapp.net").unwrap().unread,
            1,
            "the read never landed, so the chat really is unread"
        );
    }

    /// And once the store agrees, the override is gone — so a chat marked
    /// unread by hand on another device comes through rather than being
    /// papered over.
    #[test]
    fn a_manual_unread_from_another_device_survives_a_spent_override() {
        let mut bridge = bridge();
        let only = message("m1", "1@s.whatsapp.net", 10, false, true);
        bridge.observe(received("1@s.whatsapp.net", only.clone(), None));
        bridge
            .reads()
            .record_read("1@s.whatsapp.net", read_through(10, &["m1"]));

        // The read landed: the store now agrees, which spends the override.
        bridge.observe(loaded(vec![stored_chat(
            "1@s.whatsapp.net",
            0,
            vec![only.clone()],
        )]));

        // The phone marks it unread again, on the same last message.
        let mut marked = stored_chat("1@s.whatsapp.net", 0, vec![only]);
        marked.manually_unread = true;
        bridge.observe(loaded(vec![marked]));

        assert!(
            bridge.hub.chat("1@s.whatsapp.net").unwrap().manually_unread,
            "a deliberate unread elsewhere is not ours to suppress"
        );
    }

    /// One abandoned conversation must not grow the daemon without bound.
    #[test]
    fn tracked_receipts_are_capped_at_the_newest() {
        let mut bridge = bridge();
        for i in 0..(MAX_TRACKED_UNREAD + 5) {
            bridge.observe(received(
                "1@s.whatsapp.net",
                message(&format!("m{i}"), "1@s.whatsapp.net", 10, false, false),
                None,
            ));
        }
        let unread = bridge.reads().take_receipts("1@s.whatsapp.net");
        assert_eq!(unread.len(), MAX_TRACKED_UNREAD);
        assert_eq!(unread.first().unwrap().0, "m5", "the oldest went first");
    }

    /// A store reload is the store's answer for that chat: a message it now
    /// reports as read must stop being one the daemon owes a receipt for.
    #[test]
    fn a_reload_replaces_what_a_chat_still_owes() {
        let mut bridge = bridge();
        bridge.observe(received(
            "1@s.whatsapp.net",
            message("a", "1@s.whatsapp.net", 10, false, false),
            None,
        ));
        bridge.observe(loaded(vec![stored_chat(
            "1@s.whatsapp.net",
            0,
            vec![message("a", "1@s.whatsapp.net", 10, false, true)],
        )]));

        assert!(
            bridge.reads().take_receipts("1@s.whatsapp.net").is_empty(),
            "read elsewhere, so nothing is owed"
        );
    }

    /// A load that stops short of what this side has already seen says
    /// nothing about where the chat ends. Rebuilding the boundary from it let
    /// it recede, and the window's read then named a message it had just
    /// drawn and was refused for it.
    #[test]
    fn an_older_page_does_not_move_the_boundary_back() {
        let mut bridge = bridge();
        bridge.observe(received(
            "1@s.whatsapp.net",
            message("newest", "1@s.whatsapp.net", 200, false, false),
            None,
        ));

        bridge.observe(loaded(vec![stored_chat(
            "1@s.whatsapp.net",
            1,
            vec![message("older", "1@s.whatsapp.net", 100, false, false)],
        )]));

        let (secs, ids) = bridge
            .reads()
            .boundary("1@s.whatsapp.net")
            .expect("the chat still ends where it ended");
        assert_eq!(secs, 200);
        assert_eq!(
            ids.iter().map(|(id, ..)| id.as_str()).collect::<Vec<_>>(),
            ["newest"]
        );
    }

    /// An older page answers for the rows in it. Clearing the whole queue on
    /// one dropped the receipt owed for a live message the page does not
    /// carry: the boundary correctly stayed where it was, so a read naming
    /// that message was accepted, and no receipt ever went out for it.
    #[test]
    fn an_older_page_leaves_a_newer_messages_receipt_owed() {
        let mut bridge = bridge();
        bridge.observe(received(
            "1@s.whatsapp.net",
            message("newest", "1@s.whatsapp.net", 200, false, false),
            None,
        ));

        bridge.observe(loaded(vec![stored_chat(
            "1@s.whatsapp.net",
            1,
            vec![message("older", "1@s.whatsapp.net", 100, false, true)],
        )]));

        let owed = bridge.reads().take_receipts("1@s.whatsapp.net");
        assert_eq!(
            owed.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            ["newest"],
            "the page said nothing about the message it does not carry"
        );
    }

    /// A deleted chat must take its tracked ids with it, or the daemon leaks
    /// one entry per conversation that ever went away.
    #[test]
    fn a_removed_chat_takes_its_read_state_with_it() {
        let mut bridge = bridge();
        bridge.observe(loaded(vec![stored_chat(
            "1@s.whatsapp.net",
            1,
            vec![message("a", "1@s.whatsapp.net", 10, false, false)],
        )]));
        bridge
            .reads()
            .record_read("1@s.whatsapp.net", read_through(10, &["m1"]));
        assert!(bridge.reads().boundary("1@s.whatsapp.net").is_some());

        bridge.observe(loaded(Vec::new()));
        assert!(bridge.reads().boundary("1@s.whatsapp.net").is_none());
        assert!(!bridge.reads().read_through.contains_key("1@s.whatsapp.net"));
    }
}
