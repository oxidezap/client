//! The avatar descriptor row: which picture a chat is showing, and under which
//! cache key its bytes live.
//!
//! Written last in the two-phase update that changes a picture, so the row can
//! never point at bytes that did not land. Read at startup, where it is what
//! lets a window draw the picture it had before the process restarted without
//! waiting for the network.

use diesel::prelude::*;
use diesel::query_dsl::methods::FilterDsl as _;
use wacore_binary::Jid;

use crate::schema;

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
    diesel::insert_into(dsl::avatar_descriptors)
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
        ))
        // The stale write loses: a commit that lands after a newer resolution
        // must not move the row back.
        .filter(dsl::seq.lt(seq))
        .execute(conn)
        .map(|_| ())
}
