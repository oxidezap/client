//! Server acks, and the queue for the ones that arrive before the row they
//! answer. An ack names an outgoing message: it lifts that row's status and
//! carries the server's authoritative send clock.

use diesel::prelude::*;
use log::warn;
use waproto::whatsapp as wa;

use crate::schema;
use crate::store::chat_rows::reconcile_chat_head_after_timestamp_change;
use crate::store::message_rows::message_row;
use crate::store::writer::ChangeSet;

/// Which outgoing row a server ack belongs to, if one can be named.
///
/// [`NotYet`](Self::NotYet) and [`Ambiguous`](Self::Ambiguous) are both "no row
/// applied", but they must not be treated alike: only the first is answerable
/// by waiting. Deferring an ambiguous ack would hand it to whichever row next
/// claims that id — turning a deliberate refusal into a delayed mis-apply.
enum AckTarget {
    Resolved {
        chat: String,
        timestamp_ms: i64,
    },
    /// No outgoing row with this id yet. Carries the storage key the ack named,
    /// when it named one, so a deferral can be held against that chat instead
    /// of against the id alone.
    NotYet {
        chat: Option<String>,
    },
    Ambiguous,
}

fn resolve_server_ack_message(
    conn: &mut SqliteConnection,
    device_id: i32,
    ack: &wacore::types::events::ServerAck,
    cs: &mut ChangeSet,
) -> QueryResult<AckTarget> {
    use schema::messages::dsl;
    if let Some(from) = &ack.from {
        let wire = from.to_string();
        let chat = crate::lid::route_chat_key(conn, device_id, &wire, cs)?;
        let timestamp_ms: Option<i64> = message_row(device_id, &chat, &ack.id)
            .filter(dsl::from_me.eq(true))
            .select(dsl::timestamp_ms)
            .first(conn)
            .optional()?;
        if let Some(timestamp_ms) = timestamp_ms {
            return Ok(AckTarget::Resolved { chat, timestamp_ms });
        }
        // The row may sit under the peer's other identity, so retry across the
        // PN/LID pair — but ONLY that pair. Message ids are sender-chosen and
        // unique within a chat, so widening this to every chat on the device
        // would let a named ack land on an unrelated thread that happens to
        // reuse the id.
        let keys = crate::lid::chat_key_candidates(conn, device_id, &wire)?;
        let aliased: Option<(String, i64)> = dsl::messages
            .filter(
                dsl::device_id
                    .eq(device_id)
                    .and(dsl::chat_jid.eq_any(keys))
                    .and(dsl::msg_id.eq(&ack.id))
                    .and(dsl::from_me.eq(true)),
            )
            .select((dsl::chat_jid, dsl::timestamp_ms))
            .first(conn)
            .optional()?;
        return Ok(match aliased {
            Some((chat, timestamp_ms)) => AckTarget::Resolved { chat, timestamp_ms },
            None => AckTarget::NotYet { chat: Some(chat) },
        });
    }

    // Only a chatless ack falls back to the whole device, and then the id is
    // safe only when it names exactly one outgoing row.
    let matches: Vec<(String, i64)> = dsl::messages
        .filter(
            dsl::device_id
                .eq(device_id)
                .and(dsl::msg_id.eq(&ack.id))
                .and(dsl::from_me.eq(true)),
        )
        .select((dsl::chat_jid, dsl::timestamp_ms))
        .limit(2)
        .load(conn)?;
    match <[(String, i64); 1]>::try_from(matches) {
        Ok([(chat, timestamp_ms)]) => Ok(AckTarget::Resolved { chat, timestamp_ms }),
        Err(matches) if matches.is_empty() => Ok(AckTarget::NotYet { chat: None }),
        Err(_) => {
            warn!(
                target: "ChatStore/Ack",
                "Ignoring ambiguous message ack for reused id {}",
                ack.id
            );
            Ok(AckTarget::Ambiguous)
        }
    }
}

/// How long an unmatched message ack waits for its outgoing row, and how many
/// may wait at once. Both are generous relative to the window they cover (a
/// local enqueue losing to a network round trip) and small enough that a
/// pathological stream of unmatchable ids cannot grow the writer's footprint.
const DEFERRED_ACK_TTL_MS: i64 = 60_000;
const DEFERRED_ACK_CAP: usize = 64;

/// Message-class acks that arrived before their outgoing row existed.
///
/// `Event::ServerAck` is dispatched synchronously on the socket-read path,
/// while `send_message` returns at the stanza write. A host that records its
/// outgoing message *after* the send resolves — the safe order, since
/// recording first leaves a forever-pending ghost row when the send fails and
/// the store has no row delete — therefore races the ack. The window is narrow,
/// needing the local enqueue to lose to a full round trip, but the loss used to
/// be silent and permanent: the row kept its `pending` clock until some
/// delivery receipt happened to lift it (never, for an offline recipient) and
/// never picked up the server's authoritative send timestamp.
///
/// This is the same materialize-later shape the store already uses for
/// out-of-order edits and revokes, minus the placeholder row: an ack carries no
/// content, so there is nothing to show until the real insert arrives.
#[derive(Default, Clone)]
pub(crate) struct DeferredAcks {
    /// Oldest first — pushes append, so the queue is sorted by age and expiry
    /// is a prefix drain.
    entries: std::collections::VecDeque<DeferredAck>,
    /// Everything [`defer`](Self::defer) added since [`begin_batch`], kept even
    /// after `take_matching` consumes it.
    ///
    /// A batch's two kinds of mutation roll back in opposite directions. A
    /// consumption must be undone — the insert that took the ack did not
    /// survive, so the ack is still owed a row. An addition must NOT be undone:
    /// its `ServerAck` event is already off the writer channel and there is no
    /// redelivery for it, so this queue is the only remaining record. Losing it
    /// is precisely the silent, permanent drop the queue exists to prevent.
    ///
    /// [`begin_batch`]: Self::begin_batch
    added_this_batch: Vec<DeferredAck>,
}

#[derive(Clone)]
struct DeferredAck {
    deferred_at_ms: i64,
    /// Storage key the ack named, when it named one. Message ids are
    /// sender-chosen and only unique within a chat, so an ack that names its
    /// chat must only be handed to an insert into that same chat — otherwise a
    /// host reusing one id across two threads could see chat A's ack land on
    /// chat B's row. `None` (the server omitted the chat) matches on the id
    /// alone, which is the same basis its own resolution falls back to.
    ///
    /// That `None` case stays order-dependent, as the undeferred chatless path
    /// always has been: it resolves against the rows that exist when it runs,
    /// so a host that reuses one id across two chats can have the first insert
    /// take an ack the second would have made ambiguous. Closing that would
    /// mean holding every ack to the end of the batch, trading the writer's
    /// in-order application for a case that needs the host to break id
    /// uniqueness in the first place.
    chat: Option<String>,
    ack: wacore::types::events::ServerAck,
}

impl DeferredAcks {
    fn expire(&mut self, now_ms: i64) {
        while let Some(entry) = self.entries.front() {
            if now_ms.saturating_sub(entry.deferred_at_ms) < DEFERRED_ACK_TTL_MS {
                break;
            }
            warn!(
                target: "ChatStore/Ack",
                "Dropping unmatched message ack for {}: no outgoing row appeared within {}s",
                entry.ack.id,
                DEFERRED_ACK_TTL_MS / 1000
            );
            self.entries.pop_front();
        }
    }

    /// Append within the cap, evicting the oldest waiter to make room.
    fn push_bounded(&mut self, entry: DeferredAck) {
        if self.entries.len() >= DEFERRED_ACK_CAP
            && let Some(evicted) = self.entries.pop_front()
        {
            warn!(
                target: "ChatStore/Ack",
                "Dropping unmatched message ack for {}: {DEFERRED_ACK_CAP} acks already waiting",
                evicted.ack.id
            );
        }
        self.entries.push_back(entry);
    }

    /// Open a writer batch: the previous batch's additions are settled and no
    /// longer need replaying.
    pub(super) fn begin_batch(&mut self) {
        self.added_this_batch.clear();
    }

    /// Fold a batch that did not commit back onto the state it started from.
    ///
    /// The pre-batch queue is the truth for consumptions — the inserts that
    /// took those acks rolled back, so they are still owed rows. The batch's
    /// additions ride along on top, because nothing will deliver them again.
    pub(super) fn rolled_back(self, mut pre_batch: Self) -> Self {
        for entry in self.added_this_batch {
            pre_batch.push_bounded(entry);
        }
        pre_batch
    }

    pub(super) fn defer(
        &mut self,
        ack: &wacore::types::events::ServerAck,
        chat: Option<String>,
        now_ms: i64,
    ) {
        self.expire(now_ms);
        let entry = DeferredAck {
            deferred_at_ms: now_ms,
            chat,
            ack: ack.clone(),
        };
        self.added_this_batch.push(entry.clone());
        self.push_bounded(entry);
    }

    pub(super) fn take_matching(
        &mut self,
        msg_id: &str,
        chat: &str,
        now_ms: i64,
    ) -> Option<wacore::types::events::ServerAck> {
        self.expire(now_ms);
        let at = self.entries.iter().position(|entry| {
            entry.ack.id == msg_id && entry.chat.as_deref().is_none_or(|named| named == chat)
        })?;
        self.entries.remove(at).map(|entry| entry.ack)
    }
}

/// Take the deferred-ack queue, poisoned or not.
///
/// Poisoning here means the writer's transaction panicked mid-batch, and the
/// contents are precisely what has to be recovered in that case — the acks it
/// had deferred have no other record. Refusing to read them would turn the
/// panic into the silent loss the queue exists to prevent.
pub(super) fn lock_deferred_acks(
    acks: &std::sync::Mutex<DeferredAcks>,
) -> std::sync::MutexGuard<'_, DeferredAcks> {
    acks.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What became of a server ack, so the caller knows whether anything is left to
/// hold on to.
pub(super) enum AckApplied {
    /// Applied to a row, or deliberately dropped — nothing left to hold.
    Settled,
    /// The send is not recorded yet. Carries the storage key the ack named, to
    /// hold the deferral against.
    Deferrable(Option<String>),
}

pub(super) fn apply_server_ack(
    conn: &mut SqliteConnection,
    device_id: i32,
    ack: &wacore::types::events::ServerAck,
    cs: &mut ChangeSet,
) -> QueryResult<AckApplied> {
    // Acks cover every stanza class; only message acks map to a stored row.
    if ack.class.as_deref() != Some("message") {
        return Ok(AckApplied::Settled);
    }
    use schema::messages::dsl;
    let (chat, old_timestamp_ms) = match resolve_server_ack_message(conn, device_id, ack, cs)? {
        AckTarget::Resolved { chat, timestamp_ms } => (chat, timestamp_ms),
        // Answerable by waiting: the send may just not be recorded yet.
        AckTarget::NotYet { chat } => return Ok(AckApplied::Deferrable(chat)),
        // Not answerable by waiting, and dangerous to hold — report it settled
        // so the caller drops it instead of arming it for the next row that
        // reuses the id.
        AckTarget::Ambiguous => return Ok(AckApplied::Settled),
    };
    let target = message_row(device_id, &chat, &ack.id).filter(dsl::from_me.eq(true));
    let status_updated = if ack.error.is_some() {
        // Nack: the server rejected the send. Only a still-pending row fails —
        // the server emits one ack per stanza, so a row past PENDING already
        // got its positive answer and a stray nack must not regress it.
        diesel::update(target.filter(dsl::status.eq(wa::web_message_info::Status::PENDING as i32)))
            .set(dsl::status.eq(wa::web_message_info::Status::ERROR as i32))
            .execute(conn)?
            > 0
    } else {
        // `lt(SERVER_ACK)` covers PENDING and, because `ERROR` is 0, also a
        // row already recorded as failed. It is excluded: a failure is
        // terminal here, and the front end agrees (a retry is a fresh send
        // under a new id, and the original keeps its failed bubble). Without
        // this a late positive ack turned a send the user was already told
        // had failed into one shown as delivered.
        diesel::update(
            target
                .filter(dsl::status.lt(wa::web_message_info::Status::SERVER_ACK as i32))
                .filter(dsl::status.ne(wa::web_message_info::Status::ERROR as i32)),
        )
        .set(dsl::status.eq(wa::web_message_info::Status::SERVER_ACK as i32))
        .execute(conn)?
            > 0
    };
    // A positive message ack's `t` is the server's authoritative send clock.
    // Apply it independently of the status transition: a delivery/read receipt
    // may have advanced the row before the ack event reaches this writer.
    let server_timestamp_ms = ack
        .timestamp
        .filter(|_| ack.error.is_none())
        .map(|timestamp| timestamp.timestamp_millis());
    let timestamp_updated = if let Some(timestamp_ms) = server_timestamp_ms {
        diesel::update(
            message_row(device_id, &chat, &ack.id)
                .filter(dsl::from_me.eq(true))
                .filter(dsl::timestamp_ms.ne(timestamp_ms)),
        )
        .set(dsl::timestamp_ms.eq(timestamp_ms))
        .execute(conn)?
            > 0
    } else {
        false
    };
    if timestamp_updated
        && let Some(timestamp_ms) = server_timestamp_ms
        && reconcile_chat_head_after_timestamp_change(
            conn,
            device_id,
            &chat,
            old_timestamp_ms,
            timestamp_ms,
        )?
    {
        cs.chats = true;
    }
    if status_updated || timestamp_updated {
        // Resolve the chat from the row itself: the ack's `from` is the wire
        // identity, which may not be the key the row is stored under (PN/LID
        // aliasing). Emit both so consumers keyed by either get invalidated.
        cs.message_chats.insert(chat);
        if let Some(from) = &ack.from {
            cs.message_chats.insert(from.to_string());
        }
    }
    Ok(AckApplied::Settled)
}

#[cfg(test)]
mod deferred_ack_tests {
    use std::sync::Arc;

    use super::*;

    fn ack(id: &str) -> wacore::types::events::ServerAck {
        wacore::types::events::ServerAck::builder()
            .id(id.to_string())
            .class("message".to_string())
            .build()
    }

    const CHAT: &str = "559900000001@s.whatsapp.net";
    const OTHER: &str = "559900000002@s.whatsapp.net";

    #[test]
    fn takes_only_its_own_id() {
        let mut acks = DeferredAcks::default();
        acks.defer(&ack("A"), None, 0);
        acks.defer(&ack("B"), None, 0);

        assert!(acks.take_matching("C", CHAT, 0).is_none());
        assert_eq!(acks.take_matching("B", CHAT, 0).unwrap().id, "B");
        // Consumed, not merely read.
        assert!(acks.take_matching("B", CHAT, 0).is_none());
        assert_eq!(acks.take_matching("A", CHAT, 0).unwrap().id, "A");
    }

    /// Message ids are sender-chosen and unique only within a chat, so an ack
    /// that named its chat must not be handed to an insert into another one.
    #[test]
    fn a_chat_scoped_ack_ignores_the_same_id_elsewhere() {
        let mut acks = DeferredAcks::default();
        acks.defer(&ack("OUT-DUP"), Some(CHAT.to_string()), 0);

        assert!(
            acks.take_matching("OUT-DUP", OTHER, 0).is_none(),
            "another chat's insert must not consume it"
        );
        assert!(acks.take_matching("OUT-DUP", CHAT, 0).is_some());
    }

    /// An ack the server sent without a chat resolves on the id alone, so it
    /// matches whichever chat records that id.
    #[test]
    fn a_chatless_ack_matches_any_chat() {
        let mut acks = DeferredAcks::default();
        acks.defer(&ack("OUT-ANY"), None, 0);
        assert!(acks.take_matching("OUT-ANY", OTHER, 0).is_some());
    }

    #[test]
    fn drops_entries_past_the_ttl() {
        let mut acks = DeferredAcks::default();
        acks.defer(&ack("STALE"), None, 0);

        assert!(
            acks.take_matching("STALE", CHAT, DEFERRED_ACK_TTL_MS)
                .is_none()
        );
        // One millisecond inside the window still matches.
        acks.defer(&ack("FRESH"), None, 0);
        assert!(
            acks.take_matching("FRESH", CHAT, DEFERRED_ACK_TTL_MS - 1)
                .is_some()
        );
    }

    #[test]
    fn evicts_the_oldest_at_capacity() {
        let mut acks = DeferredAcks::default();
        for i in 0..DEFERRED_ACK_CAP + 1 {
            acks.defer(&ack(&format!("ACK-{i}")), None, 0);
        }
        assert!(
            acks.take_matching("ACK-0", CHAT, 0).is_none(),
            "the oldest makes room"
        );
        assert!(
            acks.take_matching(&format!("ACK-{DEFERRED_ACK_CAP}"), CHAT, 0)
                .is_some()
        );
    }

    /// A rolled-back batch undoes what it consumed: the insert that took the
    /// ack did not survive, so the ack is still owed a row.
    #[test]
    fn rollback_gives_back_a_consumed_ack() {
        let mut acks = DeferredAcks::default();
        acks.defer(&ack("OUT-1"), None, 0);

        acks.begin_batch();
        let pre_batch = acks.clone();
        assert_eq!(acks.take_matching("OUT-1", CHAT, 0).unwrap().id, "OUT-1");
        assert!(acks.take_matching("OUT-1", CHAT, 0).is_none());

        acks = acks.rolled_back(pre_batch);
        assert_eq!(acks.take_matching("OUT-1", CHAT, 0).unwrap().id, "OUT-1");
    }

    /// ...but it must NOT undo what it added. A `ServerAck` event is off the
    /// writer channel by then and never redelivered, so dropping the deferral
    /// is the silent permanent loss this whole queue exists to prevent.
    #[test]
    fn rollback_keeps_an_ack_the_batch_deferred() {
        let mut acks = DeferredAcks::default();

        acks.begin_batch();
        let pre_batch = acks.clone();
        acks.defer(&ack("OUT-NEW"), None, 0);

        acks = acks.rolled_back(pre_batch);
        assert_eq!(
            acks.take_matching("OUT-NEW", CHAT, 0).unwrap().id,
            "OUT-NEW"
        );
    }

    /// An ack deferred AND consumed inside the same failed batch loses both
    /// mutations, so it is owed a row again.
    #[test]
    fn rollback_keeps_an_ack_the_batch_deferred_then_consumed() {
        let mut acks = DeferredAcks::default();

        acks.begin_batch();
        let pre_batch = acks.clone();
        acks.defer(&ack("OUT-BOTH"), None, 0);
        assert_eq!(
            acks.take_matching("OUT-BOTH", CHAT, 0).unwrap().id,
            "OUT-BOTH"
        );

        acks = acks.rolled_back(pre_batch);
        assert_eq!(
            acks.take_matching("OUT-BOTH", CHAT, 0).unwrap().id,
            "OUT-BOTH"
        );
    }

    /// A transaction that panics poisons the queue's lock while holding acks
    /// that have no other record. Reading through the poison is the whole
    /// point: refusing would turn the panic into the silent loss.
    #[test]
    fn a_poisoned_queue_still_yields_its_acks() {
        let acks = Arc::new(std::sync::Mutex::new(DeferredAcks::default()));
        lock_deferred_acks(&acks).defer(&ack("OUT-PANIC"), None, 0);

        let poisoner = Arc::clone(&acks);
        let panicked = std::thread::spawn(move || {
            let _guard = lock_deferred_acks(&poisoner);
            panic!("writer transaction blew up mid-batch");
        })
        .join();
        assert!(panicked.is_err(), "the thread must actually panic");
        assert!(acks.is_poisoned());

        assert_eq!(
            lock_deferred_acks(&acks)
                .take_matching("OUT-PANIC", CHAT, 0)
                .unwrap()
                .id,
            "OUT-PANIC"
        );
    }

    /// A committed batch settles its additions; the next rollback must not
    /// resurrect them.
    #[test]
    fn a_new_batch_forgets_the_previous_batch_additions() {
        let mut acks = DeferredAcks::default();
        acks.begin_batch();
        acks.defer(&ack("OUT-OLD"), None, 0);
        assert_eq!(
            acks.take_matching("OUT-OLD", CHAT, 0).unwrap().id,
            "OUT-OLD"
        );

        // Next batch commits nothing of its own and rolls back.
        acks.begin_batch();
        let pre_batch = acks.clone();
        acks = acks.rolled_back(pre_batch);

        assert!(
            acks.take_matching("OUT-OLD", CHAT, 0).is_none(),
            "the previous batch committed that consumption"
        );
    }
}
