//! Storage representation of message protos: compaction and codec.
//!
//! The `messages` table is mostly protobuf (~80% of its bytes on a real
//! store), and much of that is structural duplication rather than content:
//! every reply re-embeds its parent's `quotedMessage`, and almost every
//! message carries a `MessageContextInfo` whose only field is the 32-byte
//! `messageSecret` the pipeline already captured upstream. Both are stripped
//! from the *storage copy only* — the live `wa::Message` the pipeline holds
//! is never touched — and a cheap structural gate decides per message, so
//! small messages pay nothing.
//!
//! What is deliberately NOT here: `MsgSecretPolicy::Disabled` (the secrets
//! this strips stay authoritative in `whatsapp-rust`'s own secret store under
//! `Managed`; a resolver reading secrets back out of these protos would find
//! them gone, so dropping that store needs its own equivalence proof — see
//! the note on [`strip_redundant_secret`]), and proto elision for plain text
//! (tombstones already use `proto = NULL`, so elision needs a representation
//! this schema does not have; `CODEC_SYNTHETIC_TEXT` is reserved for it).
//!
//! Edit protos are stored raw, not compacted: an edit carries the full edited
//! message (often a reply with its own quote), and compacting there would
//! thread the parent lookup through the edit path for rare, small savings.
//! `apply_edit` resets the codec to raw for the same reason — a row
//! compressed earlier must come back to raw when an edit replaces its bytes,
//! or the reader would decompress plain protobuf.

use diesel::prelude::*;
use waproto::whatsapp as wa;

use crate::schema;

/// Storage representation of the `proto` blob. Stored in `proto_codec`.
pub(crate) const CODEC_RAW: i32 = 0;
/// `proto` is zlib-compressed protobuf; decompress before
/// [`waproto::codec::message_decode`].
pub(crate) const CODEC_COMPRESSED: i32 = 1;
/// Reserved: a plain-text message reconstructed from `kind` + `text_content`
/// (`proto = NULL`). Not written — see the module docs for why.
#[allow(dead_code)]
pub(crate) const CODEC_SYNTHETIC_TEXT: i32 = 2;

/// Only protos at or above this size are even considered for compression: the
/// distribution skews so heavily (well under 1% of rows holds megabytes)
/// that everything below it is never worth a decompression on read.
pub(crate) const COMPRESS_THRESHOLD_BYTES: usize = 8 * 1024;

/// Minimum saving that justifies the CPU: the compressed form must be at most
/// 85% of the raw size.
const COMPRESS_MAX_RATIO_NUM: usize = 85;
const COMPRESS_MAX_RATIO_DEN: usize = 100;

/// The linkage a reply carries to its parent: the quoted message's id and its
/// author's participant, as the wire spelled them.
pub(crate) struct QuoteTarget {
    pub stanza_id: String,
    pub participant: String,
}

/// Run `f` on each `ContextInfo` attached to a body a reply is ever sent
/// as, stopping at the first one `f` consumes.
///
/// The same body set the session's quote reader uses (`session::quoting`),
/// in the same order: text first, then the media kinds. Anything outside
/// that set renders without a quote bar there, and keeps its inline snapshot
/// here — the two stay in agreement about what a quote is.
/// One probe of one body: when it carries a `ContextInfo`, run `f` on it and
/// keep the first answer. Bodies differ by type, so the projection cannot be
/// collected and iterated — it is repeated per body instead, and the seven
/// call sites below are the one spelling of the body list.
macro_rules! probe_body {
    ($found:ident, $msg:expr, $field:ident, $opt:ident, |$ctx:ident| $body:expr) => {
        if $found.is_none()
            && let Some(__b) = $msg.$field.$opt()
            && let Some($ctx) = __b.context_info.$opt()
        {
            $found = $body;
        }
    };
}

fn with_contexts<T>(
    msg: &wa::Message,
    mut f: impl FnMut(&wa::ContextInfo) -> Option<T>,
) -> Option<T> {
    let mut found = None;
    probe_body!(found, msg, extended_text_message, as_option, |ctx| f(ctx));
    probe_body!(found, msg, image_message, as_option, |ctx| f(ctx));
    probe_body!(found, msg, video_message, as_option, |ctx| f(ctx));
    probe_body!(found, msg, ptv_message, as_option, |ctx| f(ctx));
    probe_body!(found, msg, audio_message, as_option, |ctx| f(ctx));
    probe_body!(found, msg, document_message, as_option, |ctx| f(ctx));
    probe_body!(found, msg, sticker_message, as_option, |ctx| f(ctx));
    found
}

/// [`with_contexts`] for the stripping pass.
fn with_contexts_mut<T>(
    msg: &mut wa::Message,
    mut f: impl FnMut(&mut wa::ContextInfo) -> Option<T>,
) -> Option<T> {
    let mut found = None;
    probe_body!(found, msg, extended_text_message, as_option_mut, |ctx| f(
        ctx
    ));
    probe_body!(found, msg, image_message, as_option_mut, |ctx| f(ctx));
    probe_body!(found, msg, video_message, as_option_mut, |ctx| f(ctx));
    probe_body!(found, msg, ptv_message, as_option_mut, |ctx| f(ctx));
    probe_body!(found, msg, audio_message, as_option_mut, |ctx| f(ctx));
    probe_body!(found, msg, document_message, as_option_mut, |ctx| f(ctx));
    probe_body!(found, msg, sticker_message, as_option_mut, |ctx| f(ctx));
    found
}

/// The reply linkage on `msg`, if it carries one with an embedded snapshot.
///
/// Needs both halves: a `stanza_id` with nothing behind it is the resend
/// shape the reader already renders linkage-only, and there is nothing to
/// strip from it.
pub(crate) fn quote_snapshot(msg: &wa::Message) -> Option<QuoteTarget> {
    with_contexts(msg, |ctx| match (&ctx.stanza_id, &ctx.quoted_message) {
        (Some(stanza_id), quoted) if quoted.is_set() => Some(QuoteTarget {
            stanza_id: stanza_id.clone(),
            participant: ctx.participant.clone().unwrap_or_default(),
        }),
        _ => None,
    })
}

/// Remove the embedded snapshot from `msg`'s quote context, returning the
/// linkage and the removed parent. The caller restores the parent when the
/// store has no row for it ([`restore_quoted`]); dropping the linkage itself
/// would lose the reply-ness, the jump target, and the sender.
pub(crate) fn take_quoted(msg: &mut wa::Message) -> Option<(QuoteTarget, wa::Message)> {
    with_contexts_mut(msg, |ctx| {
        match (&ctx.stanza_id, ctx.quoted_message.take()) {
            (Some(stanza_id), Some(quoted)) => Some((
                QuoteTarget {
                    stanza_id: stanza_id.clone(),
                    participant: ctx.participant.clone().unwrap_or_default(),
                },
                quoted,
            )),
            _ => None,
        }
    })
}

/// Put a snapshot back after [`take_quoted`], for the fallback path where the
/// parent is not materialized locally. Targets the context the snapshot came
/// from by stanza id, so a message carrying two contexts cannot misfile it.
pub(crate) fn restore_quoted(msg: &mut wa::Message, stanza_id: &str, quoted: wa::Message) {
    // Moved in on the single pass the walker makes: it stops at the first
    // context the closure consumes.
    let mut slot = Some(quoted);
    with_contexts_mut(msg, |ctx| {
        if ctx.stanza_id.as_deref() != Some(stanza_id) {
            return None;
        }
        let quoted = slot.take()?;
        ctx.quoted_message = buffa::MessageField::some(quoted);
        Some(())
    });
}

/// The reply linkage on `msg`, snapshot or not: the stanza id a quote points
/// at, if any. The reader uses it to find stripped quotes worth rehydrating
/// (linkage without a snapshot).
pub(crate) fn quote_link(msg: &wa::Message) -> Option<QuoteTarget> {
    with_contexts(msg, |ctx| {
        ctx.stanza_id.clone().map(|stanza_id| QuoteTarget {
            stanza_id,
            participant: ctx.participant.clone().unwrap_or_default(),
        })
    })
}

/// Fill a stripped quote back in from the parent's stored message: the
/// in-memory copy only, never written back. Only fills a context that names
/// `stanza_id` and holds no snapshot, so a fallback inline quote is never
/// overwritten and a resend's bare linkage keeps pointing at nothing.
pub(crate) fn inject_quoted(msg: &mut wa::Message, stanza_id: &str, quoted: &wa::Message) -> bool {
    with_contexts_mut(msg, |ctx| {
        if ctx.stanza_id.as_deref() == Some(stanza_id) && !ctx.quoted_message.is_set() {
            ctx.quoted_message = buffa::MessageField::some(quoted.clone());
            Some(())
        } else {
            None
        }
    })
    .is_some()
}

/// Whether the top-level `MessageContextInfo` carries nothing but the
/// message secret: every other known field unset, and an encoded length that
/// proves nothing else rides along either.
///
/// The encoded-length half is belt-and-braces for a future `buffa` that
/// retains unknown fields; the pinned `buffa` drops them at decode (measured:
/// a secret-plus-unknown envelope re-encodes to the secret alone), which is
/// pre-existing — the writer has always re-encoded the decoded struct, strip
/// or not. Either way a bot-metadata, thread-id, or reporting-token envelope
/// is never touched.
fn is_secret_only(ctx: &wa::MessageContextInfo) -> bool {
    use buffa::Message as _;
    ctx.message_secret.is_some()
        && !ctx.device_list_metadata.is_set()
        && ctx.device_list_metadata_version.is_none()
        && ctx.padding_bytes.is_none()
        && ctx.message_add_on_duration_in_secs.is_none()
        && ctx.bot_message_secret.is_none()
        && !ctx.bot_metadata.is_set()
        && ctx.reporting_token_version.is_none()
        && ctx.message_add_on_expiry_type.is_none()
        && !ctx.message_association.is_set()
        && ctx.capi_created_group.is_none()
        && ctx.support_payload.is_none()
        && !ctx.limit_sharing.is_set()
        && !ctx.limit_sharing_v2.is_set()
        && ctx.thread_id.is_empty()
        && ctx.weblink_render_config.is_none()
        && ctx.tee_bot_metadata.is_none()
        && !ctx.account_encryption_attestation.is_set()
        && ctx.associated_primary_identity_key.is_none()
        && {
            let mut probe = ctx.clone();
            probe.message_secret = None;
            probe.encoded_len() == 0
        }
}

/// Strip a secret-only [`is_secret_only`] envelope from the storage copy.
///
/// Safe because the storage copy is never a secret source: `whatsapp-rust`
/// captures `messageSecret` into its own secret store in the receive lane
/// *before* the event reaches this crate's materializer (dispatch captures,
/// then decrypts, then dispatches), and nothing in this workspace reads the
/// secret back out of a stored proto. What stays authoritative are that
/// store under `Managed` — which is why this must be revisited, not just
/// kept, if the client ever moves to `Disabled` with a resolver reading
/// secrets out of these rows: it would find them gone.
///
/// Returns true when the message was touched.
pub(crate) fn strip_redundant_secret(msg: &mut wa::Message) -> bool {
    use buffa::Message as _;
    let Some(ctx) = msg.message_context_info.as_option_mut() else {
        return false;
    };
    if !is_secret_only(ctx) {
        return false;
    }
    ctx.message_secret = None;
    if ctx.encoded_len() == 0 {
        msg.message_context_info = Default::default();
    }
    true
}

/// Whether `msg` holds anything the storage copy would compact: a quoted
/// snapshot or a strippable secret. The writer clones only then; every other
/// message encodes straight from the reference it was handed.
pub(crate) fn needs_storage_compaction(msg: &wa::Message) -> bool {
    if quote_snapshot(msg).is_some() {
        return true;
    }
    msg.message_context_info
        .as_option()
        .is_some_and(is_secret_only)
}

/// A candidate parent row for a quote: who stored it and the bytes to
/// rehydrate from (still in their storage representation).
pub(crate) struct ParentRow {
    pub msg_id: String,
    pub sender: String,
    pub proto: Option<Vec<u8>>,
    pub codec: i32,
}

/// Every row under one stanza id in `chat` (either storage identity), oldest
/// first. Capped: ids repeat across group participants, but past a handful
/// of same-id rows the quote is ambiguous anyway and keeps its snapshot.
pub(crate) fn quote_parent_rows(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat_keys: &[String],
    stanza_id: &str,
) -> QueryResult<Vec<ParentRow>> {
    use schema::messages::dsl;
    dsl::messages
        .filter(
            dsl::device_id
                .eq(device_id)
                .and(dsl::chat_jid.eq_any(chat_keys.to_vec()))
                .and(dsl::msg_id.eq(stanza_id)),
        )
        .order(dsl::id.asc())
        .limit(4)
        .select((dsl::msg_id, dsl::sender_jid, dsl::proto, dsl::proto_codec))
        .load::<(String, String, Option<Vec<u8>>, i32)>(conn)
        .map(|rows| {
            rows.into_iter()
                .map(|(msg_id, sender, proto, codec)| ParentRow {
                    msg_id,
                    sender,
                    proto,
                    codec,
                })
                .collect()
        })
}

/// Which candidate a quote resolves to, if any.
///
/// Identity is `msg_id`-plus-author on purpose — ids are sender-chosen and
/// repeat across group participants, so a chat-wide id match alone could
/// strip a reply onto the wrong parent's existence (or hydrate it with the
/// wrong parent's text). The author matches exactly, or through the PN/LID
/// counterpart when the quote spells the sender under the peer's other
/// identity. An empty participant with exactly one row under the stanza id
/// resolves to it (own-history rows carry no participant); anything else
/// ambiguous resolves to nothing and the inline snapshot (or the bare
/// linkage) stands.
pub(crate) fn pick_quote_parent<'a>(
    rows: &'a [ParentRow],
    participant: &str,
    alias: Option<&str>,
) -> Option<&'a ParentRow> {
    if let Some(row) = rows.iter().find(|row| row.sender == participant) {
        return Some(row);
    }
    if let Some(alias) = alias
        && let Some(row) = rows.iter().find(|row| row.sender == alias)
    {
        return Some(row);
    }
    if participant.is_empty() && rows.len() == 1 {
        return rows.first();
    }
    None
}

/// Whether a row for the quoted parent exists: same chat (either storage
/// identity), same stanza id, same author (see [`pick_quote_parent`]).
pub(crate) fn quote_parent_exists(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    target: &QuoteTarget,
) -> QueryResult<bool> {
    let keys = crate::lid::chat_key_candidates(conn, device_id, chat)?;
    let rows = quote_parent_rows(conn, device_id, &keys, &target.stanza_id)?;
    // A mapping read that fails is not a write failure: without the alias
    // the quote resolves by exact author only, and the snapshot stays.
    let alias: Option<String> = if target.participant.is_empty() {
        None
    } else {
        crate::lid::counterpart_chat_key(conn, device_id, &target.participant).unwrap_or(None)
    };
    Ok(pick_quote_parent(&rows, &target.participant, alias.as_deref()).is_some())
}

/// What to persist for one message: the bytes, their representation, and —
/// for the history-sync post-pass — the quote linkage when its snapshot was
/// kept inline because the parent was not visible yet.
pub(crate) struct StorageBytes {
    pub bytes: Vec<u8>,
    pub codec: i32,
    pub kept_quote: Option<QuoteTarget>,
}

/// The bytes (and codec) to persist for `msg` in `chat`.
///
/// Messages with nothing to compact encode straight from the reference.
/// Replies with an embedded snapshot and secret-only envelopes go through an
/// owned copy: the snapshot is dropped when the parent is materialized
/// locally (the reader rehydrates it in batch — the linkage, stanza id and
/// participant stay on the row either way), kept inline otherwise, and the
/// secret envelope is stripped unconditionally (see
/// [`strip_redundant_secret`] for why that half needs no parent check).
pub(crate) fn storage_bytes_for(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    msg: &wa::Message,
) -> QueryResult<StorageBytes> {
    if !needs_storage_compaction(msg) {
        let (bytes, codec) = encode_storage_proto(msg);
        return Ok(StorageBytes {
            bytes,
            codec,
            kept_quote: None,
        });
    }
    let mut owned = msg.clone();
    let taken = take_quoted(&mut owned);
    strip_redundant_secret(&mut owned);
    let mut kept_quote = None;
    if let Some((target, quoted)) = taken
        && !quote_parent_exists(conn, device_id, chat, &target)?
    {
        restore_quoted(&mut owned, &target.stanza_id, quoted);
        kept_quote = Some(target);
    }
    let (bytes, codec) = encode_storage_proto(&owned);
    Ok(StorageBytes {
        bytes,
        codec,
        kept_quote,
    })
}

/// A reply that kept its inline snapshot because the parent was not
/// materialized yet when it stored — in practice, a parent later in the same
/// history-sync conversation. The post-pass ([`strip_pending_quotes`])
/// re-checks these once the conversation is fully inserted.
pub(crate) struct PendingQuote {
    pub msg_id: String,
    pub sender: String,
    pub target: QuoteTarget,
}

/// Second chance for [`PendingQuote`]s: after a history conversation is fully
/// inserted, parents that arrived later in the same batch are visible, so the
/// snapshots kept inline at insert time can go.
///
/// One load for all hit rows (not one per reply), then one `UPDATE` per row
/// that actually strips. Rows whose stored proto no longer decodes are left
/// alone — an inline snapshot is always a correct fallback. Replies whose
/// parent never materializes keep theirs the same way.
pub(crate) fn strip_pending_quotes(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    pending: &[PendingQuote],
) -> QueryResult<()> {
    use schema::messages::dsl;
    let mut hits: Vec<&PendingQuote> = Vec::new();
    for quote in pending {
        if quote_parent_exists(conn, device_id, chat, &quote.target)? {
            hits.push(quote);
        }
    }
    if hits.is_empty() {
        return Ok(());
    }
    let keys = crate::lid::chat_key_candidates(conn, device_id, chat)?;
    let ids: Vec<&str> = hits.iter().map(|quote| quote.msg_id.as_str()).collect();
    let mut rows: Vec<(String, String, Option<Vec<u8>>, i32)> = dsl::messages
        .filter(
            dsl::device_id
                .eq(device_id)
                .and(dsl::chat_jid.eq_any(keys))
                .and(dsl::msg_id.eq_any(ids)),
        )
        .select((dsl::msg_id, dsl::sender_jid, dsl::proto, dsl::proto_codec))
        .load(conn)?;
    // `eq_any` returns table order; the map below re-keys by identity.
    type StoredBytes = (Option<Vec<u8>>, i32);
    let mut by_identity: std::collections::HashMap<(String, String), StoredBytes> =
        std::collections::HashMap::new();
    for (msg_id, sender_jid, proto, codec) in rows.drain(..) {
        by_identity.insert((msg_id, sender_jid), (proto, codec));
    }
    for quote in hits {
        let Some((proto, codec)) = by_identity.get(&(quote.msg_id.clone(), quote.sender.clone()))
        else {
            continue;
        };
        let Some(bytes) = proto.as_deref() else {
            continue;
        };
        let Ok(mut msg) = decode_storage_proto(bytes, *codec) else {
            continue;
        };
        // The parent exists (checked above); drop this row's snapshot. A
        // concurrent edit inside the same batch would have replaced the
        // proto, and then there is nothing to strip — `take_quoted`
        // returning `None` simply skips the rewrite.
        if take_quoted(&mut msg).is_none() {
            continue;
        }
        let (stripped, stripped_codec) = encode_storage_proto(&msg);
        diesel::update(
            crate::store::message_row(device_id, chat, &quote.msg_id)
                .filter(dsl::sender_jid.eq(&quote.sender)),
        )
        .set((
            dsl::proto.eq(Some(stripped)),
            dsl::proto_codec.eq(stripped_codec),
        ))
        .execute(conn)?;
    }
    Ok(())
}

/// Compact bytes that arrived already encoded (the outgoing path encodes in
/// the public API, before the writer sees a connection). Decodes, compacts,
/// re-encodes; an undecodable input is kept byte-for-byte raw — the writer
/// cannot improve what it cannot parse, and dropping it would lose the send.
pub(crate) fn compact_encoded_proto(
    conn: &mut SqliteConnection,
    device_id: i32,
    chat: &str,
    bytes: &[u8],
) -> QueryResult<(Vec<u8>, i32)> {
    let Ok(msg) = waproto::codec::message_decode(bytes) else {
        return Ok((bytes.to_vec(), CODEC_RAW));
    };
    let stored = storage_bytes_for(conn, device_id, chat, &msg)?;
    Ok((stored.bytes, stored.codec))
}

/// Encode for storage, compressing selectively.
///
/// Only protos at or above [`COMPRESS_THRESHOLD_BYTES`] whose compressed form
/// saves at least 15% pay the codec: a fraction of a percent of rows holds
/// megabytes, and the goal is those megabytes without taxing every small
/// message with a decompression on read.
pub(crate) fn encode_storage_proto(msg: &wa::Message) -> (Vec<u8>, i32) {
    let raw = waproto::codec::message_to_vec(msg);
    if raw.len() < COMPRESS_THRESHOLD_BYTES {
        return (raw, CODEC_RAW);
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    use std::io::Write as _;
    let compressed = match encoder.write_all(&raw).and_then(|()| encoder.finish()) {
        Ok(compressed)
            if compressed.len() * COMPRESS_MAX_RATIO_DEN <= raw.len() * COMPRESS_MAX_RATIO_NUM =>
        {
            compressed
        }
        _ => return (raw, CODEC_RAW),
    };
    (compressed, CODEC_COMPRESSED)
}

/// Decode what [`encode_storage_proto`] wrote. Unknown codecs are an error —
/// the caller renders the denormalized columns without the rich proto, the
/// same fallback an undecodable blob gets.
pub(crate) fn decode_storage_proto(
    bytes: &[u8],
    codec: i32,
) -> Result<wa::Message, StorageProtoError> {
    match codec {
        CODEC_RAW => Ok(waproto::codec::message_decode(bytes)?),
        CODEC_COMPRESSED => {
            use std::io::Read as _;
            let mut decoder = flate2::read::ZlibDecoder::new(bytes);
            let mut raw = Vec::new();
            decoder.read_to_end(&mut raw)?;
            Ok(waproto::codec::message_decode(&raw)?)
        }
        unknown => Err(StorageProtoError::UnknownCodec(unknown)),
    }
}

/// What [`decode_storage_proto`] can report. Displayed in the caller's
/// undecodable-proto warning; never persisted.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StorageProtoError {
    #[error("unknown proto codec {0}")]
    UnknownCodec(i32),
    #[error("decompress: {0}")]
    Decompress(#[from] std::io::Error),
    #[error("decode: {0}")]
    Decode(#[from] buffa::DecodeError),
}

#[cfg(test)]
mod tests {
    // Tests exercise the raw buffa API (ContextInfo has no codec-level
    // helpers; only `wa::Message` does).
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use buffa::Message as _;
    use wacore::proto_helpers::MessageBuilderExt as _;

    fn text_reply(stanza_id: &str, participant: &str, quoted: wa::Message) -> wa::Message {
        wa::Message {
            extended_text_message: buffa::MessageField::some(wa::message::ExtendedTextMessage {
                text: Some("pong".into()),
                context_info: buffa::MessageField::some(wa::ContextInfo {
                    stanza_id: Some(stanza_id.into()),
                    participant: Some(participant.into()),
                    quoted_message: buffa::MessageField::some(quoted),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn take_and_restore_quote_round_trips() {
        let mut msg = text_reply("ORIG", "peer", wa::Message::text("ping"));
        let (target, quoted) = take_quoted(&mut msg).expect("snapshot");
        assert_eq!(target.stanza_id, "ORIG");
        assert_eq!(target.participant, "peer");
        assert!(quote_snapshot(&msg).is_none());
        // Linkage survives the strip.
        assert_eq!(quote_link(&msg).expect("link").stanza_id, "ORIG");
        restore_quoted(&mut msg, "ORIG", quoted);
        assert_eq!(quote_snapshot(&msg).expect("restored").stanza_id, "ORIG");
    }

    #[test]
    fn inject_never_overwrites_an_inline_snapshot() {
        let mut msg = text_reply("ORIG", "peer", wa::Message::text("old"));
        let mut other = msg.clone();
        assert!(!inject_quoted(
            &mut other,
            "OTHER",
            &wa::Message::text("new")
        ));
        assert!(!inject_quoted(&mut msg, "ORIG", &wa::Message::text("new")));
        // The inline copy wins over the injected one.
        let ctx = quote_snapshot(&msg).expect("kept");
        assert_eq!(ctx.stanza_id, "ORIG");
    }

    #[test]
    fn inject_fills_a_stripped_link() {
        let mut msg = text_reply("ORIG", "peer", wa::Message::text("ping"));
        let (_, quoted) = take_quoted(&mut msg).expect("snapshot");
        assert!(inject_quoted(&mut msg, "ORIG", &quoted));
        assert_eq!(quote_snapshot(&msg).expect("injected").stanza_id, "ORIG");
    }

    #[test]
    fn secret_only_envelope_is_stripped_entirely() {
        let mut msg = wa::Message::text("hi");
        msg.message_context_info = buffa::MessageField::some(wa::MessageContextInfo {
            message_secret: Some(vec![7u8; 32]),
            ..Default::default()
        });
        assert!(strip_redundant_secret(&mut msg));
        assert!(msg.message_context_info.as_option().is_none());
    }

    #[test]
    fn secret_envelope_with_bot_metadata_is_untouched() {
        let mut msg = wa::Message::text("hi");
        msg.message_context_info = buffa::MessageField::some(wa::MessageContextInfo {
            message_secret: Some(vec![7u8; 32]),
            bot_metadata: buffa::MessageField::some(wa::BotMetadata::default()),
            ..Default::default()
        });
        assert!(!strip_redundant_secret(&mut msg));
        let ctx = msg.message_context_info.as_option().expect("kept");
        assert_eq!(ctx.message_secret.as_deref(), Some([7u8; 32].as_slice()));
    }

    #[test]
    fn secret_envelope_with_unknown_wire_fields_still_strips() {
        let ctx = wa::MessageContextInfo {
            message_secret: Some(vec![7u8; 32]),
            ..Default::default()
        };
        // Field 99, varint, value 1 — a future field this code does not know.
        let mut bytes = ctx.encode_to_vec();
        bytes.extend_from_slice(&[0x98, 0x06, 0x01]);
        let decoded =
            wa::MessageContextInfo::decode_from_slice(&bytes).expect("decode with unknown");
        // The pinned `buffa` drops unknown fields at decode (pre-existing:
        // every stored proto is a re-encode of the decoded struct), so the
        // envelope still reads as secret-only and the strip proceeds.
        assert_eq!(decoded.encode_to_vec().len(), ctx.encode_to_vec().len());
        let mut msg = wa::Message::text("hi");
        msg.message_context_info = buffa::MessageField::some(decoded);
        assert!(strip_redundant_secret(&mut msg));
        assert!(msg.message_context_info.as_option().is_none());
    }

    #[test]
    fn codec_round_trips_small_raw_and_large_compressed() {
        let small = wa::Message::text("oi");
        let (bytes, codec) = encode_storage_proto(&small);
        assert_eq!(codec, CODEC_RAW);
        assert_eq!(decode_storage_proto(&bytes, codec).expect("decode"), small);

        let big = wa::Message::text("lorem ipsum dolor sit amet ".repeat(900));
        let (bytes, codec) = encode_storage_proto(&big);
        assert_eq!(codec, CODEC_COMPRESSED);
        assert!(bytes.len() < 8 * 1024, "must actually shrink");
        assert_eq!(decode_storage_proto(&bytes, codec).expect("decode"), big);
    }

    #[test]
    fn incompressible_large_proto_stays_raw() {
        // Pseudorandom payload above the threshold: compression cannot pay.
        let mut bytes = Vec::with_capacity(10 * 1024);
        let mut x: u64 = 0x12345678;
        for _ in 0..10 * 1024 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            bytes.push((x >> 33) as u8);
        }
        let msg = wa::Message {
            image_message: buffa::MessageField::some(wa::message::ImageMessage {
                file_sha256: Some(bytes),
                ..Default::default()
            }),
            ..Default::default()
        };
        let raw = waproto::codec::message_to_vec(&msg);
        assert!(raw.len() >= COMPRESS_THRESHOLD_BYTES);
        let (_, codec) = encode_storage_proto(&msg);
        assert_eq!(codec, CODEC_RAW, "no saving, no codec");
    }

    #[test]
    fn unknown_codec_is_an_error_not_a_panic() {
        assert!(matches!(
            decode_storage_proto(b"junk", 99),
            Err(StorageProtoError::UnknownCodec(99))
        ));
        assert!(decode_storage_proto(b"\x78\x9c Junk", CODEC_COMPRESSED).is_err());
    }

    #[test]
    fn parent_pick_prefers_exact_author_then_alias() {
        let rows = vec![
            ParentRow {
                msg_id: "X".into(),
                sender: "a@s.whatsapp.net".into(),
                proto: None,
                codec: CODEC_RAW,
            },
            ParentRow {
                msg_id: "X".into(),
                sender: "b@s.whatsapp.net".into(),
                proto: None,
                codec: CODEC_RAW,
            },
        ];
        assert_eq!(
            pick_quote_parent(&rows, "b@s.whatsapp.net", None)
                .expect("exact")
                .sender,
            "b@s.whatsapp.net"
        );
        assert_eq!(
            pick_quote_parent(&rows, "c@lid", Some("a@s.whatsapp.net"))
                .expect("alias")
                .sender,
            "a@s.whatsapp.net"
        );
        assert!(pick_quote_parent(&rows, "nobody", None).is_none());
        assert!(pick_quote_parent(&rows, "", None).is_none(), "ambiguous");
        let solo = &rows[..1];
        assert_eq!(
            pick_quote_parent(solo, "", None).expect("lone row").sender,
            "a@s.whatsapp.net"
        );
    }
}
