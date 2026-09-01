//! LID/PN identity resolution (issue #1078) and the reconcile that folds a
//! split pair back into one thread.
//!
//! Rows stored under the phone-number key before any mapping was known, and
//! traffic arriving LID-keyed afterwards: the merge has to keep one row per
//! message, the newer edit, the earlier instant for a state both sides
//! recorded, and a withdrawal over the reaction it withdraws.

// Tests exercise the raw buffa API.
#![allow(clippy::disallowed_methods)]

mod common;

use common::*;

/// The issue #1078 scenario: rows stored under the phone-number key before
/// any mapping was known, delivered/read receipts arriving LID-keyed.
#[tokio::test]
async fn lid_receipt_advances_pn_keyed_rows() {
    let (store, chat_store) = test_store().await;
    let chat = jid(PEER);

    chat_store
        .record_outgoing(
            &chat,
            "OUT-SPLIT",
            &wa::Message::text("oi"),
            ts(1_700_000_100),
        )
        .unwrap();
    feed(&chat_store, [ack("OUT-SPLIT", chat.clone())]).await;

    add_lid_mapping(&store).await;
    feed(
        &chat_store,
        [read_receipt(PEER_LID, &["OUT-SPLIT"], 1_700_000_200)],
    )
    .await;

    let msg = chat_store
        .message(&chat, "OUT-SPLIT")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.status, MessageStatus::Read);
    // No stray @lid twin was created by the receipt.
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].jid, chat);
    // Either identity reads the same thread.
    let via_lid = chat_store
        .message(&jid(PEER_LID), "OUT-SPLIT")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(via_lid.status, MessageStatus::Read);
}

/// The mirror direction: LID-keyed rows, PN-keyed receipt.
#[tokio::test]
async fn pn_receipt_advances_lid_keyed_rows() {
    let (store, chat_store) = test_store().await;

    chat_store
        .record_outgoing(
            &jid(PEER_LID),
            "OUT-L",
            &wa::Message::text("oi"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    add_lid_mapping(&store).await;
    feed(&chat_store, [read_receipt(PEER, &["OUT-L"], 1_700_000_200)]).await;

    let msg = chat_store
        .message(&jid(PEER_LID), "OUT-L")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.status, MessageStatus::Read);
}

/// Without a mapping the receipt still can't be attributed (the pre-fix
/// behavior); once the mapping is learned, a replayed receipt heals the row.
#[tokio::test]
async fn receipt_heals_once_mapping_is_learned() {
    let (store, chat_store) = test_store().await;
    let chat = jid(PEER);

    chat_store
        .record_outgoing(&chat, "OUT-H", &wa::Message::text("oi"), ts(1_700_000_100))
        .unwrap();
    feed(
        &chat_store,
        [read_receipt(PEER_LID, &["OUT-H"], 1_700_000_200)],
    )
    .await;
    let msg = chat_store.message(&chat, "OUT-H").await.unwrap().unwrap();
    assert_eq!(msg.status, MessageStatus::Pending);

    add_lid_mapping(&store).await;
    feed(
        &chat_store,
        [read_receipt(PEER_LID, &["OUT-H"], 1_700_000_300)],
    )
    .await;
    let msg = chat_store.message(&chat, "OUT-H").await.unwrap().unwrap();
    assert_eq!(msg.status, MessageStatus::Read);
}

/// With the mapping known up front, a brand-new chat is keyed by the LID (WA
/// Web `selectChatForOneOnOneMessage`) even when addressed by phone number,
/// so later LID-keyed receipts hit exactly.
#[tokio::test]
async fn known_mapping_keys_new_chat_by_lid() {
    let (store, chat_store) = test_store().await;
    add_lid_mapping(&store).await;

    chat_store
        .record_outgoing(
            &jid(PEER),
            "OUT-NEW",
            &wa::Message::text("primeira"),
            ts(1_700_000_100),
        )
        .unwrap();
    feed(
        &chat_store,
        [read_receipt(PEER_LID, &["OUT-NEW"], 1_700_000_200)],
    )
    .await;

    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].jid, jid(PEER_LID));
    // The embedder still addresses (and reads) by phone number.
    let msg = chat_store
        .message(&jid(PEER), "OUT-NEW")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.status, MessageStatus::Read);
}

/// A send that failed left the bubble spinning for good: the row goes in
/// under the peer's LID, the failure was looked for under the phone number
/// the send named, nothing matched, and nothing said so — no error state, no
/// retry, and no invalidation to redraw either.
#[tokio::test]
async fn a_failed_send_does_not_stay_pending_forever() {
    let (store, chat_store) = test_store().await;
    add_lid_mapping(&store).await;

    chat_store
        .record_outgoing(
            &jid(PEER),
            "OUT-FAIL",
            &wa::Message::text("não saiu"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    // Addressed exactly as the send was, which is all the caller holds.
    chat_store.mark_send_failed(&jid(PEER), "OUT-FAIL").unwrap();
    chat_store.flush().await.unwrap();

    let msg = chat_store
        .message(&jid(PEER), "OUT-FAIL")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.status, MessageStatus::Error);
}

/// An inbound LID-addressed message joins the peer's existing PN-keyed
/// thread instead of opening a twin chat.
#[tokio::test]
async fn inbound_lid_message_joins_existing_pn_thread() {
    let (store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("antes"),
            incoming_info(PEER, PEER, "MSG-P1", 1_700_000_000),
        )],
    )
    .await;
    add_lid_mapping(&store).await;
    feed(
        &chat_store,
        [message_event(
            wa::Message::text("depois"),
            incoming_info(PEER_LID, PEER_LID, "MSG-L1", 1_700_000_100),
        )],
    )
    .await;

    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].jid, jid(PEER));
    assert_eq!(chats[0].unread_count, 2);
    assert_eq!(chats[0].last_message_preview.as_deref(), Some("depois"));
    let messages = chat_store.messages(&jid(PEER_LID), None, 10).await.unwrap();
    assert_eq!(messages.len(), 2);
}

/// A read-self receipt arriving LID-keyed clears the PN-keyed thread's badge
/// instead of materializing an empty @lid chat row.
#[tokio::test]
async fn lid_read_self_clears_pn_thread_badge() {
    let (store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("um"),
                incoming_info(PEER, PEER, "MSG-RS-A", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("dois"),
                incoming_info(PEER, PEER, "MSG-RS-B", 1_700_000_010),
            ),
        ],
    )
    .await;
    add_lid_mapping(&store).await;
    feed(
        &chat_store,
        [receipt(
            jid(PEER_LID),
            jid(PEER_LID),
            &["MSG-RS-A", "MSG-RS-B"],
            ReceiptType::ReadSelf,
            ts(1_700_000_050),
        )],
    )
    .await;

    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].jid, jid(PEER));
    assert_eq!(chats[0].unread_count, 0);
}

/// Splits left behind by the pre-fix behavior merge on demand: messages fold
/// into the newer-activity side, the badge is recounted, sticky prefs
/// survive, and the repair is idempotent.
#[tokio::test]
async fn reconcile_merges_split_pair() {
    let (store, chat_store) = test_store().await;

    // No mapping yet: two independent chats form (the split).
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("via pn"),
                incoming_info(PEER, PEER, "MSG-SP-A", 1_700_000_000),
            ),
            Event::PinUpdate(
                wacore::types::events::PinUpdate::builder()
                    .jid(jid(PEER))
                    .timestamp(ts(1_700_000_050))
                    .action(Box::new(wa::sync_action_value::PinAction {
                        pinned: Some(true),
                    }))
                    .from_full_sync(false)
                    .build(),
            ),
            message_event(
                wa::Message::text("via lid"),
                incoming_info(PEER_LID, PEER_LID, "MSG-SP-B", 1_700_000_100),
            ),
        ],
    )
    .await;
    assert_eq!(chat_store.chats(false, 10).await.unwrap().len(), 2);

    add_lid_mapping(&store).await;
    chat_store.reconcile_chat(&jid(PEER)).unwrap();
    chat_store.flush().await.unwrap();

    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats.len(), 1);
    // Newest activity was on the LID side, so that key survives.
    assert_eq!(chats[0].jid, jid(PEER_LID));
    assert_eq!(chats[0].unread_count, 2);
    assert!(chats[0].pinned_at.is_some());
    assert_eq!(chats[0].last_message_preview.as_deref(), Some("via lid"));
    let messages = chat_store.messages(&jid(PEER), None, 10).await.unwrap();
    assert_eq!(messages.len(), 2);

    // Idempotent.
    chat_store.reconcile_chat(&jid(PEER)).unwrap();
    chat_store.flush().await.unwrap();
    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].unread_count, 2);
}

/// A receipt names the peer's device (`user:48@lid`), and that is the form
/// a reconcile can be asked for under. `merge_split_chat` took its keys raw
/// where every other entry point normalizes, so nothing was filed under the
/// name it was given, the early return fired, and the repair that was asked
/// for silently did not happen.
#[tokio::test]
async fn a_reconcile_named_by_a_device_still_merges_the_pair() {
    let (store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("via pn"),
                incoming_info(PEER, PEER, "MSG-AD-A", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("via lid"),
                incoming_info(PEER_LID, PEER_LID, "MSG-AD-B", 1_700_000_100),
            ),
        ],
    )
    .await;
    assert_eq!(chat_store.chats(false, 10).await.unwrap().len(), 2);

    add_lid_mapping(&store).await;
    let with_device: Jid = format!("{}:48@s.whatsapp.net", jid(PEER).user)
        .parse()
        .expect("valid device address");
    chat_store.reconcile_chat(&with_device).unwrap();
    chat_store.flush().await.unwrap();

    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats.len(), 1, "the pair asked about is the pair merged");
    assert_eq!(chats[0].jid, jid(PEER_LID));
}

/// Reaction timestamps are whole seconds, so adding and removing inside one
/// second ties. The merge dropped a destination row only for a *strictly*
/// newer source row, so a tie kept the emoji and threw away the tombstone
/// that cancelled it, and a removed reaction came back. The live path
/// settles the same tie the other way (`ts_ms <= ts_ms`).
#[tokio::test]
async fn a_removal_that_ties_with_its_reaction_survives_the_merge() {
    let (store, chat_store) = test_store().await;
    let alice = "559900000002@s.whatsapp.net";

    // The same message reached both sides of the split, and so did alice:
    // the removal landed on the PN side and the emoji on the LID side, which
    // newer activity makes the merge's destination, both stamped to the same
    // second.
    let react = |chat: &str, emoji: &str, id: &str| {
        message_event(
            wa::Message {
                reaction_message: MessageField::some(wa::message::ReactionMessage {
                    key: MessageField::some(wa::MessageKey {
                        id: Some("MSG-TIE".into()),
                        ..Default::default()
                    }),
                    text: Some(emoji.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            incoming_info(chat, alice, id, 1_700_000_010),
        )
    };
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("target"),
                incoming_info(PEER, PEER, "MSG-TIE", 1_700_000_000),
            ),
            react(PEER, "", "R-DEL"),
            message_event(
                wa::Message::text("target"),
                incoming_info(PEER_LID, PEER_LID, "MSG-TIE", 1_700_000_100),
            ),
            react(PEER_LID, "\u{1f44d}", "R-ADD"),
        ],
    )
    .await;

    add_lid_mapping(&store).await;
    chat_store.reconcile_chat(&jid(PEER)).unwrap();
    chat_store.flush().await.unwrap();

    assert!(
        chat_store
            .reactions(&jid(PEER), "MSG-TIE")
            .await
            .unwrap()
            .is_empty(),
        "a removal is always later than the reaction it cancels"
    );
}

/// The same message stored under both keys keeps the most advanced status
/// after the merge.
#[tokio::test]
async fn merge_advances_duplicate_row_status() {
    let (store, chat_store) = test_store().await;

    chat_store
        .record_outgoing(
            &jid(PEER),
            "OUT-DUP",
            &wa::Message::text("dup"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store
        .record_outgoing(
            &jid(PEER_LID),
            "OUT-DUP",
            &wa::Message::text("dup"),
            ts(1_700_000_000),
        )
        .unwrap();
    chat_store.flush().await.unwrap();
    // Only the LID copy saw the read receipt.
    feed(
        &chat_store,
        [read_receipt(PEER_LID, &["OUT-DUP"], 1_700_000_200)],
    )
    .await;

    add_lid_mapping(&store).await;
    chat_store.reconcile_chat(&jid(PEER)).unwrap();
    chat_store.flush().await.unwrap();

    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats.len(), 1);
    // PN side had the newer activity, so it is the surviving key…
    assert_eq!(chats[0].jid, jid(PEER));
    // …and the duplicate's Read status survived onto it.
    let msg = chat_store
        .message(&jid(PEER), "OUT-DUP")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(msg.status, MessageStatus::Read);
}

/// Both identities recorded the same state before the mapping reconciled. The
/// merge direction is decided by chat activity, which says nothing about which
/// side saw the receipt first, so the earlier instant has to be carried over
/// rather than left to whichever key wins.
#[tokio::test]
async fn merge_keeps_the_earlier_instant_for_a_state_both_sides_recorded() {
    let (store, chat_store) = test_store().await;

    chat_store
        .record_outgoing(
            &jid(PEER),
            "OUT-TS",
            &wa::Message::text("dup"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store
        .record_outgoing(
            &jid(PEER_LID),
            "OUT-TS",
            &wa::Message::text("dup"),
            ts(1_700_000_000),
        )
        .unwrap();
    chat_store.flush().await.unwrap();

    // One sender, addressing the thread by each of its identities in turn —
    // the shape of a PN/LID transition, and the only way the same
    // (message, user, state) lands under both chat keys. The LID side saw the
    // read first; the PN side, which newer activity will make the merge
    // destination, saw it later.
    let read_from_lid_addressed_to = |chat: &str, ts_secs: i64| {
        receipt(
            jid(chat),
            jid(PEER_LID),
            &["OUT-TS"],
            ReceiptType::Read,
            ts(ts_secs),
        )
    };
    feed(
        &chat_store,
        [
            read_from_lid_addressed_to(PEER_LID, 1_700_000_200),
            read_from_lid_addressed_to(PEER, 1_700_000_800),
        ],
    )
    .await;

    add_lid_mapping(&store).await;
    chat_store.reconcile_chat(&jid(PEER)).unwrap();
    chat_store.flush().await.unwrap();

    let receipts = chat_store.receipts(&jid(PEER), "OUT-TS").await.unwrap();
    assert_eq!(receipts.len(), 1, "{receipts:?}");
    assert_eq!(
        receipts[0].timestamp.timestamp(),
        1_700_000_200,
        "the losing side saw it first, so its instant is the true one"
    );
}

/// One peer, not two. A 1:1's receipt names whoever the peer sent from, so a
/// thread that changed identity mid-flight accumulates rows under both — and
/// the merge is the only place that can put them back together, since moving
/// `chat_jid` leaves `user_jid` untouched.
#[tokio::test]
async fn merge_folds_a_split_peer_identity_into_one_user() {
    let (store, chat_store) = test_store().await;

    chat_store
        .record_outgoing(
            &jid(PEER),
            "OUT-WHO",
            &wa::Message::text("dup"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store
        .record_outgoing(
            &jid(PEER_LID),
            "OUT-WHO",
            &wa::Message::text("dup"),
            ts(1_700_000_000),
        )
        .unwrap();
    chat_store.flush().await.unwrap();

    // Delivered reported from the LID identity, read from the PN one.
    feed(
        &chat_store,
        [
            peer_receipt(
                jid(PEER_LID),
                &["OUT-WHO"],
                ReceiptType::Delivered,
                1_700_000_200,
            ),
            peer_receipt(jid(PEER), &["OUT-WHO"], ReceiptType::Read, 1_700_000_300),
        ],
    )
    .await;

    add_lid_mapping(&store).await;
    chat_store.reconcile_chat(&jid(PEER)).unwrap();
    chat_store.flush().await.unwrap();

    let receipts = chat_store.receipts(&jid(PEER), "OUT-WHO").await.unwrap();
    let mut users: Vec<String> = receipts.iter().map(|r| r.user_jid.to_string()).collect();
    users.dedup();
    assert_eq!(
        users,
        vec![PEER],
        "both states belong to one peer after the merge: {receipts:?}"
    );
    assert_eq!(
        receipts
            .iter()
            .map(|r| (r.status, r.timestamp.timestamp()))
            .collect::<Vec<_>>(),
        vec![
            (MessageStatus::Delivered, 1_700_000_200),
            (MessageStatus::Read, 1_700_000_300),
        ],
        "and both states survive: {receipts:?}"
    );
}

/// A reaction left under one of the peer's identities and taken back under
/// the other stayed on screen for ever: a removal is a reaction with an empty
/// emoji, `reactions_for` filters those out, and the merge moved the rows to
/// one chat without ever agreeing they came from one person.
#[tokio::test]
async fn merge_folds_a_reactor_split_across_their_identities() {
    let (store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("alvo"),
                incoming_info(PEER, PEER, "MSG-RX", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("alvo"),
                incoming_info(PEER_LID, PEER_LID, "MSG-RX-LID", 1_699_999_000),
            ),
        ],
    )
    .await;

    let react = |chat: &str, emoji: &str, id: &str, ts: i64| {
        message_event(
            wa::Message {
                reaction_message: MessageField::some(wa::message::ReactionMessage {
                    key: MessageField::some(wa::MessageKey {
                        id: Some("MSG-RX".into()),
                        ..Default::default()
                    }),
                    text: Some(emoji.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            incoming_info(chat, chat, id, ts),
        )
    };
    feed(
        &chat_store,
        [
            react(PEER, "👍", "RX1", 1_700_000_010),
            // Taken back, from the same person addressing themselves the
            // other way.
            react(PEER_LID, "", "RX2", 1_700_000_020),
        ],
    )
    .await;

    add_lid_mapping(&store).await;
    chat_store.reconcile_chat(&jid(PEER)).unwrap();
    chat_store.flush().await.unwrap();

    let reactions = chat_store.reactions(&jid(PEER), "MSG-RX").await.unwrap();
    assert!(
        reactions.is_empty(),
        "the reaction was withdrawn: {reactions:?}"
    );
}

/// A reaction and its withdrawal are exactly what land under two identities
/// inside one tick, and the identity is the wrong thing to settle that with:
/// ordered by JID, whichever sorts higher wins, so the reaction survives its
/// own removal half the time. The rule the same-sender fold already uses —
/// the empty emoji wins a tie — has to survive the cross-identity one.
#[tokio::test]
async fn a_withdrawal_beats_its_reaction_in_the_same_tick() {
    let (store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("alvo"),
                incoming_info(PEER, PEER, "MSG-TIE", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("alvo"),
                incoming_info(PEER_LID, PEER_LID, "MSG-TIE-LID", 1_699_999_000),
            ),
        ],
    )
    .await;

    let react = |chat: &str, emoji: &str, id: &str| {
        message_event(
            wa::Message {
                reaction_message: MessageField::some(wa::message::ReactionMessage {
                    key: MessageField::some(wa::MessageKey {
                        id: Some("MSG-TIE".into()),
                        ..Default::default()
                    }),
                    text: Some(emoji.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            // The same second on both, which is the tie.
            incoming_info(chat, chat, id, 1_700_000_010),
        )
    };
    // The tombstone under the identity that sorts *lower*, so an ordering by
    // JID alone would keep the reaction it withdraws.
    feed(
        &chat_store,
        [react(PEER, "👍", "TIE1"), react(PEER_LID, "", "TIE2")],
    )
    .await;

    add_lid_mapping(&store).await;
    chat_store.reconcile_chat(&jid(PEER)).unwrap();
    chat_store.flush().await.unwrap();

    let reactions = chat_store.reactions(&jid(PEER), "MSG-TIE").await.unwrap();
    assert!(
        reactions.is_empty(),
        "the withdrawal is the later word even when the clock cannot say so: {reactions:?}"
    );
}

/// The collision the identity rewrite cannot resolve by itself: both
/// identities recorded the *same* state, and one of the rows already sits
/// under the surviving key. Renaming it would duplicate the row that is
/// already there, so it is skipped — and the `chat_jid = src` sweep never
/// reaches it, because it was never filed under `src`.
#[tokio::test]
async fn merge_drops_the_twin_left_by_a_same_state_collision() {
    let (store, chat_store) = test_store().await;

    chat_store
        .record_outgoing(
            &jid(PEER),
            "OUT-TWIN",
            &wa::Message::text("dup"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store
        .record_outgoing(
            &jid(PEER_LID),
            "OUT-TWIN",
            &wa::Message::text("dup"),
            ts(1_700_000_000),
        )
        .unwrap();
    chat_store.flush().await.unwrap();

    let read_from = |sender: &str, chat: &str, ts_secs: i64| {
        receipt(
            jid(chat),
            jid(sender),
            &["OUT-TWIN"],
            ReceiptType::Read,
            ts(ts_secs),
        )
    };
    // Same state, same surviving chat, two identities — and the retiring
    // identity is the one that saw it first.
    feed(
        &chat_store,
        [
            read_from(PEER_LID, PEER, 1_700_000_200),
            read_from(PEER, PEER, 1_700_000_800),
        ],
    )
    .await;

    add_lid_mapping(&store).await;
    chat_store.reconcile_chat(&jid(PEER)).unwrap();
    chat_store.flush().await.unwrap();

    let receipts = chat_store.receipts(&jid(PEER), "OUT-TWIN").await.unwrap();
    assert_eq!(
        receipts.len(),
        1,
        "the skipped twin must not outlive the merge: {receipts:?}"
    );
    assert_eq!(receipts[0].user_jid, jid(PEER));
    assert_eq!(
        receipts[0].timestamp.timestamp(),
        1_700_000_200,
        "and it leaves its instant behind"
    );
}

/// The same collision from the retiring side. Here the skipped row sits under
/// `src`, so the dedup sweep must reach it before the chat rename does —
/// otherwise the rename carries it to the surviving thread untouched, still
/// naming the identity being retired.
#[tokio::test]
async fn merge_drops_a_same_state_collision_left_on_the_retiring_side() {
    let (store, chat_store) = test_store().await;

    chat_store
        .record_outgoing(
            &jid(PEER),
            "OUT-SRC-TWIN",
            &wa::Message::text("dup"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store
        .record_outgoing(
            &jid(PEER_LID),
            "OUT-SRC-TWIN",
            &wa::Message::text("dup"),
            ts(1_700_000_000),
        )
        .unwrap();
    chat_store.flush().await.unwrap();

    let read_from = |sender: &str, chat: &str, ts_secs: i64| {
        receipt(
            jid(chat),
            jid(sender),
            &["OUT-SRC-TWIN"],
            ReceiptType::Read,
            ts(ts_secs),
        )
    };
    // Both identities record the same state under the LID chat — the side that
    // newer PN activity will retire.
    feed(
        &chat_store,
        [
            read_from(PEER_LID, PEER_LID, 1_700_000_800),
            read_from(PEER, PEER_LID, 1_700_000_200),
        ],
    )
    .await;

    add_lid_mapping(&store).await;
    chat_store.reconcile_chat(&jid(PEER)).unwrap();
    chat_store.flush().await.unwrap();

    let receipts = chat_store
        .receipts(&jid(PEER), "OUT-SRC-TWIN")
        .await
        .unwrap();
    assert_eq!(
        receipts.len(),
        1,
        "the collision survivor must not ride the rename over: {receipts:?}"
    );
    assert_eq!(receipts[0].user_jid, jid(PEER));
    assert_eq!(
        receipts[0].timestamp.timestamp(),
        1_700_000_200,
        "and the earlier instant survives"
    );
}

/// A reaction addressed by the peer's other identity lands on the stored
/// message (routing picks the existing thread).
#[tokio::test]
async fn lid_reaction_reaches_pn_keyed_message() {
    let (store, chat_store) = test_store().await;

    feed(
        &chat_store,
        [message_event(
            wa::Message::text("alvo"),
            incoming_info(PEER, PEER, "MSG-R1", 1_700_000_000),
        )],
    )
    .await;
    add_lid_mapping(&store).await;

    let reaction = wa::Message {
        reaction_message: MessageField::some(wa::message::ReactionMessage {
            key: MessageField::some(wa::MessageKey {
                remote_jid: Some(PEER_LID.to_string()),
                from_me: Some(false),
                id: Some("MSG-R1".to_string()),
                ..Default::default()
            }),
            text: Some("👍".to_string()),
            sender_timestamp_ms: Some(1_700_000_100_000),
            ..Default::default()
        }),
        ..Default::default()
    };
    feed(
        &chat_store,
        [message_event(
            reaction,
            incoming_info(PEER_LID, PEER_LID, "MSG-R1-REACT", 1_700_000_100),
        )],
    )
    .await;

    let reactions = chat_store.reactions(&jid(PEER), "MSG-R1").await.unwrap();
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0].emoji, "👍");
    // No twin chat was opened for the reaction.
    assert_eq!(chat_store.chats(false, 10).await.unwrap().len(), 1);
}

/// An edit or revoke that reached only one side of a split survives the
/// merge: the source copy's newer edit content and tombstone fold into the
/// surviving row (same monotonic rules as the live path).
#[tokio::test]
async fn merge_folds_src_side_edit_and_revoke() {
    let (store, chat_store) = test_store().await;

    // No mapping: duplicate copies of both messages under each identity.
    for chat in [PEER, PEER_LID] {
        feed(
            &chat_store,
            [
                message_event(
                    wa::Message::text("typo"),
                    incoming_info(chat, chat, "MSG-ED", 1_700_000_000),
                ),
                message_event(
                    wa::Message::text("apaga"),
                    incoming_info(chat, chat, "MSG-RV", 1_700_000_010),
                ),
            ],
        )
        .await;
    }
    // Edit and revoke land only on the LID side.
    let edit = wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some("MSG-ED".into()),
                ..Default::default()
            }),
            r#type: Some(wa::message::protocol_message::Type::MESSAGE_EDIT),
            edited_message: MessageField::from_box(Box::new(wa::Message::text("consertada"))),
            ..Default::default()
        }),
        ..Default::default()
    };
    let revoke = revoke("MSG-RV");
    feed(
        &chat_store,
        [
            message_event(
                edit,
                incoming_info(PEER_LID, PEER_LID, "MSG-ED2", 1_700_000_050),
            ),
            message_event(
                revoke,
                incoming_info(PEER_LID, PEER_LID, "MSG-RV2", 1_700_000_060),
            ),
            // Newer activity on the PN side makes it the merge destination.
            message_event(
                wa::Message::text("mais nova"),
                incoming_info(PEER, PEER, "MSG-NW", 1_700_000_100),
            ),
        ],
    )
    .await;

    add_lid_mapping(&store).await;
    chat_store.reconcile_chat(&jid(PEER)).unwrap();
    chat_store.flush().await.unwrap();

    let chats = chat_store.chats(false, 10).await.unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].jid, jid(PEER));
    let edited = chat_store
        .message(&jid(PEER), "MSG-ED")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(edited.text.as_deref(), Some("consertada"));
    assert!(edited.edited_at.is_some());
    let revoked = chat_store
        .message(&jid(PEER), "MSG-RV")
        .await
        .unwrap()
        .unwrap();
    assert!(revoked.revoked);
    assert!(revoked.text.is_none());
}

/// Competing edits on both copies of the same message: the strictly newer
/// edit wins the merge in either direction.
#[tokio::test]
async fn merge_keeps_strictly_newer_edit_across_sides() {
    let (store, chat_store) = test_store().await;

    // No mapping: duplicate copies under each identity, then both sides edit.
    for chat in [PEER, PEER_LID] {
        feed(
            &chat_store,
            [
                message_event(
                    wa::Message::text("v0-a"),
                    incoming_info(chat, chat, "MSG-CE1", 1_700_000_000),
                ),
                message_event(
                    wa::Message::text("v0-b"),
                    incoming_info(chat, chat, "MSG-CE2", 1_700_000_010),
                ),
            ],
        )
        .await;
    }
    let edit = |target: &str, text: &str| wa::Message {
        protocol_message: MessageField::some(wa::message::ProtocolMessage {
            key: MessageField::some(wa::MessageKey {
                id: Some(target.into()),
                ..Default::default()
            }),
            r#type: Some(wa::message::protocol_message::Type::MESSAGE_EDIT),
            edited_message: MessageField::from_box(Box::new(wa::Message::text(text))),
            ..Default::default()
        }),
        ..Default::default()
    };
    feed(
        &chat_store,
        [
            // CE1: the PN (destination) side carries the newer edit.
            message_event(
                edit("MSG-CE1", "lid antiga"),
                incoming_info(PEER_LID, PEER_LID, "MSG-CE1-EL", 1_700_000_050),
            ),
            message_event(
                edit("MSG-CE1", "pn mais nova"),
                incoming_info(PEER, PEER, "MSG-CE1-EP", 1_700_000_080),
            ),
            // CE2: the LID (source) side carries the newer edit.
            message_event(
                edit("MSG-CE2", "pn antiga"),
                incoming_info(PEER, PEER, "MSG-CE2-EP", 1_700_000_050),
            ),
            message_event(
                edit("MSG-CE2", "lid mais nova"),
                incoming_info(PEER_LID, PEER_LID, "MSG-CE2-EL", 1_700_000_080),
            ),
            // Newest activity keeps the PN side as merge destination.
            message_event(
                wa::Message::text("mais nova"),
                incoming_info(PEER, PEER, "MSG-CE-NW", 1_700_000_100),
            ),
        ],
    )
    .await;

    add_lid_mapping(&store).await;
    chat_store.reconcile_chat(&jid(PEER)).unwrap();
    chat_store.flush().await.unwrap();

    let ce1 = chat_store
        .message(&jid(PEER), "MSG-CE1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ce1.text.as_deref(), Some("pn mais nova"));
    assert_eq!(
        ce1.edited_at.map(|t| t.timestamp()),
        Some(1_700_000_080),
        "destination's newer edit must not be clobbered by the source's older one"
    );
    let ce2 = chat_store
        .message(&jid(PEER), "MSG-CE2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ce2.text.as_deref(), Some("lid mais nova"));
    assert_eq!(ce2.edited_at.map(|t| t.timestamp()), Some(1_700_000_080));
}

/// `Error` sits below `Pending` on WhatsApp's own scale, so folding a split
/// by the raw number promoted a send that had failed for good back to
/// "sending" — where nothing was ever going to move it again.
#[tokio::test]
async fn merging_a_split_does_not_put_a_failed_send_back_in_flight() {
    let (store, chat_store) = test_store().await;

    // The LID side first and older, so the merge keeps the PN side.
    chat_store
        .record_outgoing(
            &jid(PEER_LID),
            "OUT-SPLIT",
            &wa::Message::text("oi"),
            ts(1_700_000_000),
        )
        .unwrap();
    chat_store
        .record_outgoing(
            &jid(PEER),
            "OUT-SPLIT",
            &wa::Message::text("oi"),
            ts(1_700_000_100),
        )
        .unwrap();
    chat_store
        .mark_send_failed(&jid(PEER), "OUT-SPLIT")
        .unwrap();
    chat_store.flush().await.unwrap();
    assert_eq!(
        chat_store.messages(&jid(PEER), None, 10).await.unwrap()[0].status,
        MessageStatus::Error
    );

    add_lid_mapping(&store).await;
    chat_store.reconcile_chat(&jid(PEER)).unwrap();
    chat_store.flush().await.unwrap();

    let merged = chat_store.messages(&jid(PEER), None, 10).await.unwrap();
    let row = merged
        .iter()
        .find(|m| m.id == "OUT-SPLIT")
        .expect("the send");
    assert_eq!(
        row.status,
        MessageStatus::Error,
        "a failure outranks a send still in flight"
    );
}

/// A read covers both halves of a PN/LID pair, and the message key includes
/// the chat: until a split is merged the same message exists under both. Drawn
/// twice it fills two slots of the page's limit and moves the cursor past a
/// row nobody was shown.
#[tokio::test]
async fn a_split_pair_does_not_hand_back_the_same_message_twice() {
    let (store, chat_store) = test_store().await;
    // No mapping yet, so the two identities form two threads.
    feed(
        &chat_store,
        [
            message_event(
                wa::Message::text("via pn"),
                incoming_info(PEER, PEER, "MSG-DUP-1", 1_700_000_000),
            ),
            message_event(
                wa::Message::text("via lid"),
                incoming_info(PEER_LID, PEER_LID, "MSG-DUP-1", 1_700_000_000),
            ),
        ],
    )
    .await;
    assert_eq!(chat_store.chats(false, 10).await.unwrap().len(), 2);

    add_lid_mapping(&store).await;
    let messages = chat_store.messages(&jid(PEER), None, 10).await.unwrap();
    assert_eq!(
        messages.iter().filter(|m| m.id == "MSG-DUP-1").count(),
        1,
        "one message, whichever key it is filed under"
    );
}

/// A page is `limit` messages, not `limit` rows. Collapsing a duplicate
/// inside the page made it shorter than it asked for, and a page shorter than
/// its limit is how the caller recognises the start of a conversation: the
/// history ended at the split, with older messages unreachable.
#[tokio::test]
async fn a_collapsed_duplicate_does_not_shorten_the_page() {
    let (store, chat_store) = test_store().await;
    // No mapping yet, so the two identities file their own copy of the pair.
    let mut events = Vec::new();
    for (n, id) in ["DUP-A", "DUP-B"].iter().enumerate() {
        let at = 1_700_000_000 + n as i64;
        events.push(message_event(
            wa::Message::text("under the number"),
            incoming_info(PEER, PEER, id, at),
        ));
        events.push(message_event(
            wa::Message::text("under the lid"),
            incoming_info(PEER_LID, PEER_LID, id, at),
        ));
    }
    events.push(message_event(
        wa::Message::text("older, and only under the number"),
        incoming_info(PEER, PEER, "OLDER", 1_699_999_000),
    ));
    feed(&chat_store, events).await;

    add_lid_mapping(&store).await;
    let page = chat_store.messages(&jid(PEER), None, 3).await.unwrap();
    assert_eq!(
        page.len(),
        3,
        "a duplicate collapsed inside the page is topped up from behind it"
    );
    assert!(
        page.iter().any(|m| m.id == "OLDER"),
        "and the row behind the duplicates is what fills the slot"
    );
}

/// The same rule for the batch an attach load asks for. This one is worse
/// than a short page: the attach limit is sized to cover the unread tail, so
/// a message dropped out of it never reaches `ReadTracker` and its receipt is
/// never sent — the badge comes back on the next hydration, for a message the
/// person has read.
#[tokio::test]
async fn a_collapsed_duplicate_does_not_shorten_a_batched_page() {
    let (store, chat_store) = test_store().await;
    let mut events = Vec::new();
    for (n, id) in ["BATCH-A", "BATCH-B"].iter().enumerate() {
        let at = 1_700_000_000 + n as i64;
        events.push(message_event(
            wa::Message::text("under the number"),
            incoming_info(PEER, PEER, id, at),
        ));
        events.push(message_event(
            wa::Message::text("under the lid"),
            incoming_info(PEER_LID, PEER_LID, id, at),
        ));
    }
    events.push(message_event(
        wa::Message::text("older, and only under the number"),
        incoming_info(PEER, PEER, "BATCH-OLDER", 1_699_999_000),
    ));
    feed(&chat_store, events).await;

    add_lid_mapping(&store).await;
    let pages = chat_store.pages(vec![(jid(PEER), 3)]).await.unwrap();
    let page = pages
        .values()
        .next()
        .expect("the chat has rows under one of its two keys");
    assert_eq!(
        page.len(),
        3,
        "the batch fills to its limit in unique messages, as one chat's page does"
    );
    assert!(
        page.iter().any(|m| m.id == "BATCH-OLDER"),
        "and it is the row behind the duplicates that fills the slot"
    );
}
