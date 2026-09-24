diesel::table! {
    chats (device_id, jid) {
        device_id -> Integer,
        jid -> Text,
        name -> Nullable<Text>,
        last_message_ts -> BigInt,
        last_message_preview -> Nullable<Text>,
        last_message_kind -> Nullable<Text>,
        unread_count -> Integer,
        pinned_at -> Nullable<BigInt>,
        muted_until -> Nullable<BigInt>,
        archived -> Bool,
        ephemeral_expiration -> Nullable<Integer>,
        read_boundary_ms -> BigInt,
        read_boundary_ids -> Nullable<Text>,
        mute_appstate_seen -> Bool,
        archive_appstate_seen -> Bool,
    }
}

diesel::table! {
    messages (id) {
        // Explicit, persisted alias of the rowid: the arrival counter the
        // readers sort and page on, by a name a VACUUM or table rewrite
        // preserves. Never written: the INSERT assigns it (`max(id) + 1`)
        // and every UPDATE the writer does leaves it alone, which is exactly
        // the "order the socket delivered this row" the message sort needs.
        id -> BigInt,
        device_id -> Integer,
        chat_jid -> Text,
        msg_id -> Text,
        sender_jid -> Text,
        from_me -> Bool,
        timestamp_ms -> BigInt,
        kind -> Text,
        text_content -> Nullable<Text>,
        proto -> Nullable<Binary>,
        // Storage representation of `proto`: 0 = raw protobuf,
        // 1 = zlib-compressed protobuf. See `proto_codec`.
        proto_codec -> Integer,
        status -> Integer,
        starred -> Bool,
        edited_at_ms -> Nullable<BigInt>,
        revoked -> Bool,
    }
}

diesel::table! {
    reactions (device_id, chat_jid, msg_id, sender_jid) {
        device_id -> Integer,
        chat_jid -> Text,
        msg_id -> Text,
        sender_jid -> Text,
        emoji -> Text,
        ts_ms -> BigInt,
    }
}

diesel::table! {
    contacts (device_id, jid) {
        device_id -> Integer,
        jid -> Text,
        push_name -> Nullable<Text>,
        full_name -> Nullable<Text>,
        first_name -> Nullable<Text>,
        business_name -> Nullable<Text>,
    }
}

diesel::table! {
    contact_labels (device_id, jid) {
        device_id -> Integer,
        jid -> Text,
        alias -> Nullable<Text>,
        tags -> Text,
    }
}

diesel::table! {
    message_receipts (device_id, chat_jid, msg_id, user_jid, receipt_type) {
        device_id -> Integer,
        chat_jid -> Text,
        msg_id -> Text,
        user_jid -> Text,
        receipt_type -> Integer,
        ts_ms -> BigInt,
    }
}

diesel::table! {
    media_refs (device_id, file_sha256) {
        device_id -> Integer,
        file_sha256 -> Binary,
        file_path -> Text,
        mime_type -> Nullable<Text>,
        size_bytes -> Nullable<BigInt>,
        downloaded_at_ms -> BigInt,
    }
}

// Not ours: the device store owns this table, and this crate only reads it.
// Declared rather than spelled as SQL text at every call, so a column renamed
// under us is a compile error here and in `lid.rs` instead of a query that
// parses fine and fails inside the writer loop, where a failed batch stops
// history materializing with nothing on screen to say so.
diesel::table! {
    lid_pn_mapping (device_id, lid) {
        device_id -> Integer,
        lid -> Text,
        phone_number -> Text,
        updated_at -> BigInt,
    }
}

diesel::table! {
    message_identity_repair_state (device_id) {
        device_id -> Integer,
        mapping_revision -> BigInt,
        repaired_revision -> BigInt,
        full_repair_pending -> Bool,
    }
}

diesel::table! {
    message_identity_repair_pending (device_id, lid) {
        device_id -> Integer,
        lid -> Text,
    }
}

diesel::table! {
    avatar_descriptors (device_id, jid) {
        device_id -> Integer,
        jid -> Text,
        picture_id -> Text,
        cache_key -> Text,
        updated_at_ms -> BigInt,
        seq -> BigInt,
    }
}

diesel::allow_tables_to_appear_in_same_query!(
    chats,
    messages,
    reactions,
    contacts,
    contact_labels,
    message_receipts,
    message_identity_repair_state,
    message_identity_repair_pending
);
