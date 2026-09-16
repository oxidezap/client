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

pub(super) fn upsert(
    conn: &mut SqliteConnection,
    device_id: i32,
    jid: &Jid,
    picture_id: &str,
    cache_key: &str,
) -> QueryResult<()> {
    use schema::avatar_descriptors::dsl;
    let jid = jid.to_string();
    let now_ms = wacore::time::now_utc().timestamp_millis();
    diesel::insert_into(dsl::avatar_descriptors)
        .values((
            dsl::device_id.eq(device_id),
            dsl::jid.eq(&jid),
            dsl::picture_id.eq(picture_id),
            dsl::cache_key.eq(cache_key),
            dsl::updated_at_ms.eq(now_ms),
        ))
        .on_conflict((dsl::device_id, dsl::jid))
        .do_update()
        .set((
            dsl::picture_id.eq(picture_id),
            dsl::cache_key.eq(cache_key),
            dsl::updated_at_ms.eq(now_ms),
        ))
        .execute(conn)
        .map(|_| ())
}

pub(super) fn delete(conn: &mut SqliteConnection, device_id: i32, jid: &Jid) -> QueryResult<()> {
    use schema::avatar_descriptors::dsl;
    let jid = jid.to_string();
    diesel::delete(
        dsl::avatar_descriptors.filter(dsl::device_id.eq(device_id).and(dsl::jid.eq(jid))),
    )
    .execute(conn)
    .map(|_| ())
}
