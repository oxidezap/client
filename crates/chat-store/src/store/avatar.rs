//! The avatar descriptor row: which picture a chat is showing, and under which
//! cache key its bytes live.
//!
//! Written last in the two-phase update that changes a picture, so the row can
//! never point at bytes that did not land. Read at startup, where it is what
//! lets a window draw the picture it had before the process restarted without
//! waiting for the network.

use diesel::prelude::*;
use wacore_binary::Jid;

use crate::schema;

const ORDER_FLOOR: &str = "__oxidezap_avatar_order_floor__";
const MAX_TOMBSTONES: i64 = 256;

/// A fixed watermark protects compacted removals. An older insert for an
/// absent descriptor is conservatively refused; its caller can retry with a
/// new resolution. Existing rows still use their per-chat sequence guard.
fn order_floor(conn: &mut SqliteConnection, device_id: i32) -> QueryResult<i64> {
    use schema::avatar_descriptors::dsl;
    dsl::avatar_descriptors
        .filter(dsl::device_id.eq(device_id).and(dsl::jid.eq(ORDER_FLOOR)))
        .select(dsl::seq)
        .first::<i64>(conn)
        .optional()
        .map(|seq| seq.unwrap_or(-1))
}

pub(super) fn compact(conn: &mut SqliteConnection, device_id: i32) -> QueryResult<()> {
    use schema::avatar_descriptors::dsl;
    let count: i64 = dsl::avatar_descriptors
        .filter(
            dsl::device_id
                .eq(device_id)
                .and(dsl::picture_id.eq(""))
                .and(dsl::jid.ne(ORDER_FLOOR)),
        )
        .count()
        .get_result(conn)?;
    if count <= MAX_TOMBSTONES {
        return Ok(());
    }
    let highest: Option<i64> = dsl::avatar_descriptors
        .filter(dsl::device_id.eq(device_id).and(dsl::picture_id.eq("")))
        .select(diesel::dsl::max(dsl::seq))
        .first(conn)?;
    let floor = highest.unwrap_or(0).max(order_floor(conn, device_id)?);
    diesel::insert_into(dsl::avatar_descriptors)
        .values((
            dsl::device_id.eq(device_id),
            dsl::jid.eq(ORDER_FLOOR),
            dsl::picture_id.eq(""),
            dsl::cache_key.eq(""),
            dsl::updated_at_ms.eq(0i64),
            dsl::seq.eq(floor),
        ))
        .on_conflict((dsl::device_id, dsl::jid))
        .do_update()
        .set(dsl::seq.eq(floor))
        .execute(conn)?;
    retire_through(conn, device_id, floor)
}

/// Repair pointers left by an earlier runtime that compacted only removals.
/// Run once at account open, rather than scanning derived pointers per removal.
pub(super) fn recover(conn: &mut SqliteConnection, device_id: i32) -> QueryResult<()> {
    compact(conn, device_id)?;
    let floor = order_floor(conn, device_id)?;
    if floor < 0 {
        return Ok(());
    }
    retire_through(conn, device_id, floor)
}

fn retire_through(conn: &mut SqliteConnection, device_id: i32, floor: i64) -> QueryResult<()> {
    use schema::avatar_descriptors::dsl;
    // Unknown aliases can be linked after this removal is compacted.
    // Retire older derived picture pointers too, so they cannot resurface.
    // Visible images and media bytes stay usable; a later lookup revalidates.
    diesel::delete(diesel::QueryDsl::filter(
        dsl::avatar_descriptors,
        dsl::device_id
            .eq(device_id)
            .and(dsl::jid.ne(ORDER_FLOOR))
            .and(dsl::seq.le(floor)),
    ))
    .execute(conn)
    .map(|_| ())
}

/// Store a chat's picture, unless a newer resolution already won.
///
/// `seq` orders resolutions by when they were made, not by when they commit:
/// two pictures resolved in quick succession come from separate tasks and
/// their commits can reach here out of order, so the row keeps whichever
/// resolution is newer rather than whichever write arrived last. Without this
/// a restart could draw the picture that lost the race.
pub(super) fn upsert(
    conn: &mut SqliteConnection,
    device_id: i32,
    jid: &Jid,
    picture_id: &str,
    cache_key: &str,
    seq: u64,
) -> QueryResult<()> {
    use schema::avatar_descriptors::dsl;
    let jid = jid.to_string();
    let now_ms = wacore::time::now_utc().timestamp_millis();
    let seq = i64::try_from(seq).unwrap_or(i64::MAX);
    if !picture_id.is_empty() && seq <= order_floor(conn, device_id)? {
        let present = dsl::avatar_descriptors
            .filter(
                dsl::device_id
                    .eq(device_id)
                    .and(dsl::jid.eq(&jid))
                    .and(dsl::picture_id.ne("")),
            )
            .select(dsl::seq)
            .first::<i64>(conn)
            .optional()?
            .is_some();
        if !present {
            return Ok(());
        }
    }
    let update = diesel::insert_into(dsl::avatar_descriptors)
        .values((
            dsl::device_id.eq(device_id),
            dsl::jid.eq(&jid),
            dsl::picture_id.eq(picture_id),
            dsl::cache_key.eq(cache_key),
            dsl::updated_at_ms.eq(now_ms),
            dsl::seq.eq(seq),
        ))
        .on_conflict((dsl::device_id, dsl::jid))
        .do_update()
        .set((
            dsl::picture_id.eq(picture_id),
            dsl::cache_key.eq(cache_key),
            dsl::updated_at_ms.eq(now_ms),
            dsl::seq.eq(seq),
        ));
    // A delayed write must not replace a newer selection.
    diesel::query_dsl::methods::FilterDsl::filter(update, dsl::seq.lt(seq)).execute(conn)?;
    if picture_id.is_empty() {
        compact(conn, device_id)
    } else {
        Ok(())
    }
}

/// Drop a chat's descriptor, unless a newer resolution already replaced it.
///
/// The `seq` guard is what keeps a removal from deleting a picture that was
/// resolved after the removal was decided: the row only goes if it is still
/// the one the removal named.
pub(super) fn delete(
    conn: &mut SqliteConnection,
    device_id: i32,
    jid: &Jid,
    seq: u64,
) -> QueryResult<()> {
    // Keep the ordering barrier after removal. Physically deleting the row
    // lets a delayed older insert recreate a picture that was removed.
    for key in crate::lid::chat_key_candidates(conn, device_id, &jid.to_string())? {
        if let Ok(alias) = key.parse::<Jid>() {
            upsert(conn, device_id, &alias, "", "", seq)?;
        }
    }
    Ok(())
}
