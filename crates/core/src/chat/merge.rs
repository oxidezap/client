//! Folding traffic into a conversation: what a new message does to it, what a
//! page of hydrated history does to it, and what a receipt does to a row.

use std::sync::Arc;

use super::Chat;
use super::message::ChatMessage;
use crate::message_status::MessageStatus;

impl Chat {
    /// Add a message to the chat, maintaining chronological order by timestamp.
    /// Returns true when the message became the chat's newest content, so the
    /// caller knows whether to bump the chat in the list; duplicates and older
    /// backfills return false.
    pub fn add_message(&mut self, mut message: ChatMessage) -> bool {
        self.name_quoted_author(&mut message);
        // Redelivery of a message we already show (live traffic overlapping
        // hydrated history): no duplicate bubble, no recount. Id-only, not
        // (timestamp, id): the optimistic bubble's UI clock and the store's
        // commit clock can stamp the same message a second apart.
        if self.messages.iter().any(|m| m.id == message.id) {
            return false;
        }

        // Sorted insert (out-of-order decryption during history sync); equal
        // timestamps tie-break on message ID for stable ordering.
        let pos = self
            .messages
            .binary_search_by(|m| {
                m.timestamp
                    .cmp(&message.timestamp)
                    .then_with(|| m.id.cmp(&message.id))
            })
            .unwrap_or_else(|pos| pos);

        // >= on purpose: WhatsApp timestamps are second-granular, so a live
        // same-second sibling still takes over the preview.
        let is_newer_or_same = self
            .last_message_time
            .map(|t| message.timestamp >= t)
            .unwrap_or(true);

        // Unread is not gated on recency: history hydration never goes through
        // here (insert_history_message/merge_history) and the dup guard above
        // blocks redelivery recounts, so every incoming message that reaches
        // this point is new even when it lands behind the newest bubble
        // (offline catch-up, out-of-order decryption).
        //
        // It *is* gated on `is_read`, the same way the daemon gates it. A row
        // that arrives already read is not unread, and the case that made this
        // matter is the call record: a call you just finished, placed, or
        // declined is written into the conversation as an incoming row, and
        // badging the chat for an event the user was party to is nonsense. A
        // missed call still arrives unread, which is the one that earns a
        // badge.
        if !message.is_from_me && !message.is_read {
            // Saturating for the reason `StateSnapshot::total_unread` gives
            // for its own sum: a pathological count must not render as a
            // small number, and here a raw add would panic in debug and wrap
            // in release before it ever got there.
            self.unread_count = self.unread_count.saturating_add(1);
        }

        if is_newer_or_same {
            self.last_message = Some(message.preview_text());
            self.last_message_time = Some(message.timestamp);
        }

        self.messages.insert(pos, message);
        is_newer_or_same
    }

    /// Fold a freshly hydrated copy of this chat (from the durable store) into
    /// the live one. Messages merge dedup-guarded without unread bumps; the
    /// store's counters are authoritative after a flush.
    pub fn merge_history(&mut self, hydrated: Chat) {
        // A live-created chat adopted by a store load becomes prunable: the
        // store owns it from here on.
        self.from_store |= hydrated.from_store;
        self.set_name_if_not_worse(hydrated.name, hydrated.name_priority);
        // Read before the messages are moved out: it decides what an absent
        // preview below means.
        let hydrated_has_messages = !hydrated.messages.is_empty();
        for msg in hydrated.messages {
            self.insert_history_message(msg);
        }
        self.unread_count = hydrated.unread_count;
        self.manually_unread = hydrated.manually_unread;
        if hydrated.last_message_time >= self.last_message_time {
            // What an absent preview means depends on whether the load
            // brought messages. With messages, the store simply has no TEXT
            // for the newest one (a photo with no caption, a tombstone) while
            // the live label describes that same message — taking the `None`
            // would render the row as "No messages" over a chat that plainly
            // has one. With no messages, the chat really was emptied (cleared,
            // or its last message deleted, here or on another device) and the
            // live label is the stale one. The timestamp moves either way: a
            // cleared chat keeps its activity time on purpose.
            if hydrated.last_message.is_some() || !hydrated_has_messages {
                self.last_message = hydrated.last_message;
            }
            self.last_message_time = hydrated.last_message_time;
        }
    }

    /// Rename a message (optimistic local id -> real WhatsApp id),
    /// re-inserting so the (timestamp, id) sort invariant holds for
    /// same-second siblings. The renamed bubble replaces a row already
    /// present under the new id (server echo of the same message).
    pub fn rename_message(&mut self, old_id: &str, new_id: &str) -> bool {
        let Some(pos) = self.messages.iter().position(|m| m.id == old_id) else {
            return false;
        };
        let mut msg = self.messages.remove(pos);
        msg.id = new_id.to_string();
        self.insert_history_message(msg);
        true
    }

    /// Insert a hydrated message in order, without touching unread counters
    /// or the preview. An id match replaces the live bubble: the store is
    /// authoritative (edits and revokes materialize there), so the hydrated
    /// copy must not be dropped in favor of stale content.
    /// Answers whether the timeline actually moved.
    ///
    /// A page re-fetched over rows it already holds is the common case — the
    /// daemon publishes a history load on every ack and receipt, and the open
    /// conversation asks again from the top — and a caller that invalidated
    /// its rows for one of those rebuilt and re-measured the whole timeline
    /// for nothing.
    pub fn insert_history_message(&mut self, mut message: ChatMessage) -> bool {
        self.name_quoted_author(&mut message);
        // Id-only match: the hydrated copy may carry a slightly different
        // timestamp than the optimistic bubble. Remove-and-reinsert keeps
        // the (timestamp, id) sort invariant when the timestamp shifted.
        let mut unchanged = false;
        if let Some(pos) = self.messages.iter().position(|m| m.id == message.id) {
            let existing = self.messages.remove(pos);
            // The store never holds downloaded media bytes — hydrated rows
            // come back empty or with just a preview thumbnail; graft the
            // bytes the live bubble already fetched so a reload can't
            // downgrade a full download.
            if let Some(new_media) = message.media.as_mut()
                && let Some(old_media) = existing.media.as_ref()
                && !old_media.data.is_empty()
                && (new_media.data.is_empty()
                    || (new_media.data_is_preview && !old_media.data_is_preview))
            {
                new_media.data = Arc::clone(&old_media.data);
                new_media.mime_type = old_media.mime_type.clone();
                new_media.data_is_preview = old_media.data_is_preview;
            }
            // Live-only state the store doesn't carry must also survive the
            // replace: a delivery state the hydrated row has not caught up
            // with, and the push name on group bubbles. `advance` is what
            // makes this safe in both directions — whichever side is further
            // along wins, so a reload can neither un-fail a send nor pull a
            // read bubble back to delivered.
            message.status.advance(existing.status);
            if message.sender_name.is_none() {
                message.sender_name = existing.sender_name.clone();
            }
            unchanged = existing == message;
        }
        let pos = self
            .messages
            .binary_search_by(|m| {
                m.timestamp
                    .cmp(&message.timestamp)
                    .then_with(|| m.id.cmp(&message.id))
            })
            .unwrap_or_else(|pos| pos);
        self.messages.insert(pos, message);
        !unchanged
    }

    /// Mark all incoming messages as read and clear the unread badge.
    /// Outgoing bubbles are untouched: their `is_read` means "the peer read
    /// it" (delivery ticks), which opening the chat locally must not fake.
    pub fn mark_as_read(&mut self) {
        self.unread_count = 0;
        self.manually_unread = false;
        for msg in &mut self.messages {
            if !msg.is_from_me {
                msg.is_read = true;
            }
        }
    }

    /// Mark specific messages as read by their IDs.
    ///
    /// Only touches incoming messages: an outgoing bubble's ticks say what the
    /// *peer* did, which a local read cannot speak for. Receipts about our own
    /// messages go through [`Self::advance_status`] instead.
    ///
    /// Returns the number of messages that were actually updated.
    pub fn mark_messages_as_read(&mut self, message_ids: &[String]) -> usize {
        let mut count = 0;
        for msg in &mut self.messages {
            if !msg.is_from_me && message_ids.contains(&msg.id) && !msg.is_read {
                msg.is_read = true;
                count += 1;
                if self.unread_count > 0 {
                    self.unread_count -= 1;
                }
            }
        }
        count
    }

    /// Advance the delivery state of our own messages, never regressing one.
    ///
    /// Returns how many bubbles actually changed, so a receipt that tells us
    /// nothing new does not buy a re-render.
    pub fn advance_status(&mut self, message_ids: &[String], status: MessageStatus) -> usize {
        let mut count = 0;
        for msg in &mut self.messages {
            if msg.is_from_me && message_ids.contains(&msg.id) {
                let before = msg.status;
                msg.status.advance(status);
                if msg.status != before {
                    count += 1;
                }
            }
        }
        count
    }

    /// Mark one of our messages failed, for a send that never landed.
    pub fn mark_send_failed(&mut self, message_id: &str) -> bool {
        self.messages
            .iter_mut()
            .find(|m| m.id == message_id && m.is_from_me)
            // Only a row still in flight. A `SendFailed` can arrive *after*
            // the server's own acknowledgement — the send future returns its
            // error late — and regressing a Sent, Delivered or Read bubble to
            // Failed contradicts an answer the server already gave. The
            // store's writer refuses the same regression for the same reason;
            // this is the front end keeping the same rule.
            .filter(|m| m.status.is_pending())
            .map(|m| m.status.advance(MessageStatus::Failed))
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::media::make_media;
    use crate::chat::message::make_message;
    use chrono::{TimeZone, Utc};

    /// A row that arrives already read is not unread. The case that made this
    /// matter is the call record: it is written as an incoming message, so a
    /// call the user had just finished raised a badge for itself.
    #[test]
    fn a_row_that_arrives_read_raises_no_badge() {
        let mut chat = Chat::new("a@s.whatsapp.net".to_string());

        let mut seen = ChatMessage::new_incoming("call-1".into(), "a".into(), String::new());
        seen.is_read = true;
        chat.add_message(seen);
        assert_eq!(chat.unread_count, 0);

        chat.add_message(ChatMessage::new_incoming(
            "m1".into(),
            "a".into(),
            "hi".into(),
        ));
        assert_eq!(chat.unread_count, 1, "a real arrival still counts");
    }

    /// The total this feeds saturates and says why: a pathological count must
    /// not render as a small number. The increment did not, so it panicked in
    /// debug and wrapped to 0 in release before the total ever saw it.
    #[test]
    fn an_arrival_at_the_ceiling_does_not_wrap_the_badge() {
        let mut chat = Chat::new("a@s.whatsapp.net".to_string());
        chat.unread_count = u32::MAX;

        chat.add_message(ChatMessage::new_incoming(
            "m1".into(),
            "a".into(),
            "hi".into(),
        ));
        assert_eq!(chat.unread_count, u32::MAX);
    }

    /// A late `SendFailed` must not overwrite an answer the server already
    /// gave. The store's writer refuses the same regression.
    #[test]
    fn a_failure_after_the_acknowledgement_does_not_regress_the_row() {
        let mut chat = Chat::new("a@s.whatsapp.net".to_string());
        let mut sent = make_message("m1", 10);
        sent.is_from_me = true;
        sent.status = MessageStatus::Delivered;
        chat.messages.push(sent);

        assert!(!chat.mark_send_failed("m1"), "nothing to fail");
        assert_eq!(chat.messages[0].status, MessageStatus::Delivered);
    }

    #[test]
    fn a_failure_while_still_pending_marks_the_row() {
        let mut chat = Chat::new("a@s.whatsapp.net".to_string());
        let mut pending = make_message("m1", 10);
        pending.is_from_me = true;
        chat.messages.push(pending);

        assert!(chat.mark_send_failed("m1"));
        assert_eq!(chat.messages[0].status, MessageStatus::Failed);
    }

    #[test]
    fn better_name_survives_history_merges() {
        let jid = "111222333444555@lid".to_string();
        let mut chat = Chat::new(jid.clone());

        chat.merge_history(Chat::with_name_priority(
            jid.clone(),
            "Masked label".to_string(),
            1,
        ));
        assert_eq!(chat.name, "Masked label");

        chat.merge_history(Chat::with_name_priority(
            jid.clone(),
            "Fictitious Contact".to_string(),
            3,
        ));
        chat.merge_history(Chat::with_name_priority(
            jid.clone(),
            "Renamed Fictitious Contact".to_string(),
            3,
        ));
        chat.merge_history(Chat::with_name_priority(
            jid,
            "Older history label".to_string(),
            1,
        ));
        assert_eq!(chat.name, "Renamed Fictitious Contact");
    }

    /// The store keeps no preview text for a message that has none (a photo
    /// with no caption), so hydration used to blank the label the live path
    /// had already derived — and the chat list renders an absent preview as
    /// "No messages".
    #[test]
    fn hydration_without_a_preview_keeps_the_live_label() {
        let jid = "12025550143@s.whatsapp.net".to_string();
        let mut chat = Chat::new(jid.clone());
        let mut photo = make_message("M1", 1000);
        photo.content = String::new();
        photo.media = Some(make_media(vec![1, 2, 3], false));
        chat.add_message(photo);
        assert_eq!(chat.last_message.as_deref(), Some("📷 Photo"));

        // What the store hands back: the message row is there (with its media
        // envelope and no text), the preview column is not.
        let mut hydrated = Chat::new(jid);
        hydrated.from_store = true;
        let mut stored_photo = make_message("M1", 1000);
        stored_photo.content = String::new();
        stored_photo.media = Some(make_media(Vec::new(), false));
        hydrated.insert_history_message(stored_photo);
        hydrated.last_message_time = chat.last_message_time;
        hydrated.last_message = None;
        chat.merge_history(hydrated);

        assert_eq!(chat.last_message.as_deref(), Some("📷 Photo"));
        assert!(chat.last_message_time.is_some());
    }

    /// Clearing a chat (or deleting its last message) elsewhere leaves the
    /// store with an activity time and nothing to show. Keeping the live label
    /// there would leave the deleted message on the row for good.
    #[test]
    fn hydration_with_no_messages_clears_a_stale_preview() {
        let jid = "12025550143@s.whatsapp.net".to_string();
        let mut chat = Chat::new(jid.clone());
        chat.add_message(make_message("M1", 1000));
        assert!(chat.last_message.is_some());

        let mut hydrated = Chat::new(jid);
        hydrated.from_store = true;
        // The chat row survives the clear and keeps its activity time; the
        // messages are gone, so the preview column is NULL and the page empty.
        hydrated.last_message_time = chat.last_message_time;
        hydrated.last_message = None;
        chat.merge_history(hydrated);

        assert_eq!(chat.last_message, None);
    }

    /// A preview the store does have still wins: it is the newer truth, and an
    /// edit or a newly arrived message is exactly how it changes.
    #[test]
    fn hydration_with_a_preview_replaces_the_live_one() {
        let jid = "12025550143@s.whatsapp.net".to_string();
        let mut chat = Chat::new(jid.clone());
        chat.add_message(make_message("M1", 1000));

        let mut hydrated = Chat::new(jid);
        hydrated.from_store = true;
        hydrated.last_message_time = Some(Utc.timestamp_opt(2000, 0).unwrap());
        hydrated.last_message = Some("edited".to_string());
        chat.merge_history(hydrated);

        assert_eq!(chat.last_message.as_deref(), Some("edited"));
        assert_eq!(
            chat.last_message_time,
            Some(Utc.timestamp_opt(2000, 0).unwrap())
        );
    }

    #[test]
    fn store_provenance_sticks_after_history_merge() {
        // Live-created chats are not store-originated...
        let mut chat = Chat::new("12025550143@s.whatsapp.net".to_string());
        assert!(!chat.from_store);

        // ...until a history load adopts them; from there they stay prunable
        // even when a later merge comes from a live-built Chat value.
        let mut hydrated = Chat::new("12025550143@s.whatsapp.net".to_string());
        hydrated.from_store = true;
        chat.merge_history(hydrated);
        assert!(chat.from_store);

        chat.merge_history(Chat::new("12025550143@s.whatsapp.net".to_string()));
        assert!(chat.from_store);
    }

    #[test]
    fn test_messages_ordered_by_timestamp_when_added_in_order() {
        let mut chat = Chat::new("test@s.whatsapp.net".to_string());

        chat.add_message(make_message("1", 1000));
        chat.add_message(make_message("2", 2000));
        chat.add_message(make_message("3", 3000));

        assert_eq!(chat.messages.len(), 3);
        assert_eq!(chat.messages[0].id, "1");
        assert_eq!(chat.messages[1].id, "2");
        assert_eq!(chat.messages[2].id, "3");
    }

    #[test]
    fn test_messages_ordered_by_timestamp_when_added_out_of_order() {
        let mut chat = Chat::new("test@s.whatsapp.net".to_string());

        // Simulate history sync where messages are decrypted out of order
        chat.add_message(make_message("2", 2000)); // Middle message first
        chat.add_message(make_message("3", 3000)); // Newest message second
        chat.add_message(make_message("1", 1000)); // Oldest message last

        assert_eq!(chat.messages.len(), 3);
        // Should be sorted by timestamp, not insertion order
        assert_eq!(chat.messages[0].id, "1"); // oldest
        assert_eq!(chat.messages[1].id, "2"); // middle
        assert_eq!(chat.messages[2].id, "3"); // newest
    }

    #[test]
    fn test_out_of_order_incoming_still_counts_as_unread() {
        let mut chat = Chat::new("test@s.whatsapp.net".to_string());

        chat.add_message(make_message("2", 2000));
        // Lands behind the newest bubble (offline catch-up / late decrypt):
        // no preview takeover, but the badge must still count it.
        let bumped = chat.add_message(make_message("1", 1000));

        assert!(!bumped);
        assert_eq!(chat.unread_count, 2);
        // Redelivery of the same id must not recount.
        chat.add_message(make_message("1", 1000));
        assert_eq!(chat.unread_count, 2);
    }

    #[test]
    fn test_messages_ordered_by_timestamp_reverse_order() {
        let mut chat = Chat::new("test@s.whatsapp.net".to_string());

        // Add messages in reverse chronological order (newest first)
        chat.add_message(make_message("3", 3000));
        chat.add_message(make_message("2", 2000));
        chat.add_message(make_message("1", 1000));

        assert_eq!(chat.messages.len(), 3);
        assert_eq!(chat.messages[0].id, "1");
        assert_eq!(chat.messages[1].id, "2");
        assert_eq!(chat.messages[2].id, "3");
    }

    #[test]
    fn test_messages_with_same_timestamp_sorted_by_id() {
        let mut chat = Chat::new("test@s.whatsapp.net".to_string());

        // Messages with same timestamp should be sorted by ID for stable ordering
        chat.add_message(make_message("c", 1000));
        chat.add_message(make_message("a", 1000));
        chat.add_message(make_message("b", 1000));

        assert_eq!(chat.messages.len(), 3);
        // Same timestamp: sorted alphabetically by ID
        assert_eq!(chat.messages[0].id, "a");
        assert_eq!(chat.messages[1].id, "b");
        assert_eq!(chat.messages[2].id, "c");
    }

    #[test]
    fn test_rename_message_keeps_same_second_sort_order() {
        let mut chat = Chat::new("test@s.whatsapp.net".to_string());

        chat.add_message(make_message("3EB0BBB", 1000));
        chat.add_message(make_message("local_1000_0", 1000));
        // 'l' > '3', so the optimistic bubble sits after its sibling
        assert_eq!(chat.messages[1].id, "local_1000_0");

        assert!(chat.rename_message("local_1000_0", "3EB0AAA"));
        // The real id sorts before the sibling; a plain in-place rename
        // would have left the vector mis-sorted for binary_search_by
        assert_eq!(chat.messages[0].id, "3EB0AAA");
        assert_eq!(chat.messages[1].id, "3EB0BBB");

        // A later same-second insert still lands in the right slot
        chat.add_message(make_message("3EB0AB0", 1000));
        let ids: Vec<_> = chat.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["3EB0AAA", "3EB0AB0", "3EB0BBB"]);
    }

    #[test]
    fn test_rename_message_dedups_against_existing_real_id() {
        let mut chat = Chat::new("test@s.whatsapp.net".to_string());

        chat.add_message(make_message("3EB0AAA", 1000));
        chat.add_message(make_message("local_1000_0", 1000));

        // The real id already arrived (e.g. server echo); the rename must
        // not create a duplicate bubble. The renamed local bubble replaces
        // the echo row — same message, and the local copy is the one that
        // may hold media bytes.
        assert!(chat.rename_message("local_1000_0", "3EB0AAA"));
        assert_eq!(chat.messages.len(), 1);
        assert_eq!(chat.messages[0].id, "3EB0AAA");
        assert_eq!(chat.messages[0].content, "Message local_1000_0");

        assert!(!chat.rename_message("missing", "whatever"));
    }

    /// A page that repeats rows the conversation already holds is the common
    /// case: the daemon publishes a history load on every ack and receipt,
    /// and a conversation whose whole history fit in one page asks for that
    /// page again each time. A caller that could not tell used to rebuild and
    /// re-measure the entire timeline for it.
    #[test]
    fn a_row_that_arrives_again_unchanged_says_nothing_moved() {
        let mut chat = Chat::new("test@s.whatsapp.net".to_string());
        assert!(
            chat.insert_history_message(make_message("3EB0AAA", 1000)),
            "the first copy is news"
        );
        assert!(
            !chat.insert_history_message(make_message("3EB0AAA", 1000)),
            "the same row again is not"
        );
        assert_eq!(chat.messages.len(), 1);

        let mut edited = make_message("3EB0AAA", 1000);
        edited.content = "edited".to_string();
        assert!(
            chat.insert_history_message(edited),
            "a row that changed is news again"
        );
    }

    #[test]
    fn test_hydration_replaces_same_id_at_different_timestamp() {
        let mut chat = Chat::new("test@s.whatsapp.net".to_string());

        // Optimistic bubble stamped with the UI clock, renamed to the real id
        chat.add_message(make_message("local_1000_0", 1000));
        assert!(chat.rename_message("local_1000_0", "3EB0AAA"));

        // The store commits its own slightly-later timestamp; the hydrated
        // copy must replace the existing bubble (store is authoritative for
        // content — edits/revokes materialize there), not sit next to it
        let mut hydrated = make_message("3EB0AAA", 1001);
        hydrated.content = "edited text".to_string();
        chat.insert_history_message(hydrated);
        assert_eq!(chat.messages.len(), 1);
        assert_eq!(
            chat.messages[0].timestamp,
            Utc.timestamp_opt(1001, 0).unwrap()
        );
        assert_eq!(chat.messages[0].content, "edited text");

        // Live redelivery with the shifted timestamp dedups the same way
        assert!(!chat.add_message(make_message("3EB0AAA", 1001)));
        assert_eq!(chat.messages.len(), 1);
        assert_eq!(chat.messages[0].content, "edited text");
    }

    #[test]
    fn test_hydration_replacement_keeps_downloaded_media_bytes() {
        let mut chat = Chat::new("test@s.whatsapp.net".to_string());

        // Live bubble whose full media bytes were already downloaded
        let mut live = make_message("3EB0AAA", 1000);
        live.media = Some(make_media(vec![1, 2, 3], false));
        chat.add_message(live);

        // The hydrated copy carries no media bytes (the store never holds
        // them) but newer content; the replace must graft the old bytes
        let mut hydrated = make_message("3EB0AAA", 1000);
        hydrated.content = "edited caption".to_string();
        hydrated.media = Some(make_media(Vec::new(), true));
        chat.insert_history_message(hydrated);

        assert_eq!(chat.messages.len(), 1);
        assert_eq!(chat.messages[0].content, "edited caption");
        let media = chat.messages[0].media.as_ref().unwrap();
        assert_eq!(*media.data, vec![1, 2, 3]);
        assert!(!media.data_is_preview);
    }

    #[test]
    fn test_hydration_replacement_keeps_full_bytes_over_incoming_preview() {
        let mut chat = Chat::new("test@s.whatsapp.net".to_string());

        // Full media bytes already downloaded on the live bubble
        let mut live = make_message("3EB0AAA", 1000);
        let mut full = make_media(vec![1, 2, 3], false);
        full.mime_type = "image/png".to_string();
        live.media = Some(full);
        chat.add_message(live);

        // Hydrated image rows carry a jpeg thumbnail flagged as preview; the
        // replace must not downgrade the full download to it
        let mut hydrated = make_message("3EB0AAA", 1000);
        hydrated.media = Some(make_media(vec![9], true));
        chat.insert_history_message(hydrated);

        assert_eq!(chat.messages.len(), 1);
        let media = chat.messages[0].media.as_ref().unwrap();
        assert_eq!(*media.data, vec![1, 2, 3]);
        assert_eq!(media.mime_type, "image/png");
        assert!(!media.data_is_preview);
    }

    #[test]
    fn test_hydration_replacement_full_bytes_win_over_existing_preview() {
        let mut chat = Chat::new("test@s.whatsapp.net".to_string());

        // Live bubble degraded to a thumbnail (eager download failed)
        let mut live = make_message("3EB0AAA", 1000);
        live.media = Some(make_media(vec![9], true));
        chat.add_message(live);

        // Incoming copy carrying real bytes must replace the preview
        let mut hydrated = make_message("3EB0AAA", 1000);
        hydrated.media = Some(make_media(vec![1, 2, 3, 4], false));
        chat.insert_history_message(hydrated);

        assert_eq!(chat.messages.len(), 1);
        let media = chat.messages[0].media.as_ref().unwrap();
        assert_eq!(*media.data, vec![1, 2, 3, 4]);
        assert!(!media.data_is_preview);
    }

    #[test]
    fn test_hydration_replacement_keeps_delivery_state_and_sender_name() {
        let mut chat = Chat::new("123456789-group@g.us".to_string());

        // Live bubble: an outgoing send that failed, plus an incoming group
        // bubble that carried its sender's push name
        let mut failed_send = make_message("OUT-1", 1000);
        failed_send.is_from_me = true;
        failed_send.status = MessageStatus::Failed;
        chat.add_message(failed_send);
        let mut incoming = make_message("IN-1", 2000);
        incoming.sender_name = Some("Alice".to_string());
        chat.add_message(incoming);

        // The hydrated rows carry neither the failure state nor the push
        // name; the replace must not lose either
        let mut hydrated_send = make_message("OUT-1", 1000);
        hydrated_send.is_from_me = true;
        chat.insert_history_message(hydrated_send);
        chat.insert_history_message(make_message("IN-1", 2000));

        assert_eq!(chat.messages.len(), 2);
        assert!(chat.messages[0].is_failed());
        assert_eq!(chat.messages[1].sender_name.as_deref(), Some("Alice"));

        // A hydrated name wins over the live one (store is authoritative)
        let mut renamed = make_message("IN-1", 2000);
        renamed.sender_name = Some("Alice Example".to_string());
        chat.insert_history_message(renamed);
        assert_eq!(
            chat.messages[1].sender_name.as_deref(),
            Some("Alice Example")
        );
    }

    #[test]
    fn test_mark_as_read_leaves_outgoing_delivery_state_alone() {
        let mut chat = Chat::new("test@s.whatsapp.net".to_string());
        let mut outgoing = make_message("out", 1000);
        outgoing.is_from_me = true;
        chat.add_message(outgoing);
        chat.add_message(make_message("in", 2000));
        chat.manually_unread = true;

        chat.mark_as_read();

        assert_eq!(chat.unread_count, 0);
        assert!(!chat.manually_unread);
        // Outgoing is_read renders as the peer-read ticks; opening the chat
        // must not fabricate them
        assert!(!chat.messages[0].is_read);
        assert!(chat.messages[1].is_read);
    }

    #[test]
    fn test_history_sync_batch_simulation() {
        let mut chat = Chat::new("test@s.whatsapp.net".to_string());

        // Simulate a realistic history sync batch where messages arrive
        // in random order due to parallel decryption
        let messages_in_arrival_order = vec![
            ("msg5", 5000),
            ("msg2", 2000),
            ("msg8", 8000),
            ("msg1", 1000),
            ("msg4", 4000),
            ("msg7", 7000),
            ("msg3", 3000),
            ("msg6", 6000),
        ];

        for (id, ts) in messages_in_arrival_order {
            chat.add_message(make_message(id, ts));
        }

        assert_eq!(chat.messages.len(), 8);

        // Verify messages are in chronological order
        let expected_order = [
            "msg1", "msg2", "msg3", "msg4", "msg5", "msg6", "msg7", "msg8",
        ];
        for (i, expected_id) in expected_order.iter().enumerate() {
            assert_eq!(
                chat.messages[i].id, *expected_id,
                "Message at index {} should be {} but was {}",
                i, expected_id, chat.messages[i].id
            );
        }

        // Verify timestamps are actually ascending
        for i in 1..chat.messages.len() {
            assert!(
                chat.messages[i].timestamp >= chat.messages[i - 1].timestamp,
                "Messages should be in ascending timestamp order"
            );
        }
    }

    #[test]
    fn test_new_message_inserted_at_correct_position() {
        let mut chat = Chat::new("test@s.whatsapp.net".to_string());

        // Add some historical messages
        chat.add_message(make_message("old1", 1000));
        chat.add_message(make_message("old2", 2000));
        chat.add_message(make_message("old3", 3000));

        // Now receive a new real-time message
        chat.add_message(make_message("new", 4000));

        assert_eq!(chat.messages.len(), 4);
        assert_eq!(chat.messages[3].id, "new"); // Should be at the end

        // Add a late-arriving historical message
        chat.add_message(make_message("late_history", 1500));

        assert_eq!(chat.messages.len(), 5);
        assert_eq!(chat.messages[0].id, "old1");
        assert_eq!(chat.messages[1].id, "late_history"); // Inserted in correct position
        assert_eq!(chat.messages[2].id, "old2");
        assert_eq!(chat.messages[3].id, "old3");
        assert_eq!(chat.messages[4].id, "new");
    }
}
