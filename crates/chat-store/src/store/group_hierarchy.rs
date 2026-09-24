//! Persist authoritative community relationships without treating missing
//! metadata as an unlink.

use diesel::prelude::*;
use wacore_binary::JidExt as _;

use crate::schema;
use crate::store::writer::ChangeSet;
use crate::types::GroupHierarchyWrite;

pub(super) fn apply_group_hierarchies(
    conn: &mut SqliteConnection,
    device_id: i32,
    writes: &[GroupHierarchyWrite],
    cs: &mut ChangeSet,
) -> QueryResult<()> {
    use schema::chats::dsl;

    for write in writes {
        if !write.jid.is_group() || write.expected.as_ref() == Some(&write.hierarchy) {
            continue;
        }
        let jid = write.jid.to_non_ad_string();
        let expected = write
            .expected
            .as_ref()
            .map(|hierarchy| serde_json::to_string(hierarchy).expect("hierarchy serializes"));
        let hierarchy = serde_json::to_string(&write.hierarchy).expect("hierarchy serializes");
        let row = dsl::chats.filter(dsl::device_id.eq(device_id).and(dsl::jid.eq(jid)));
        let updated = match expected {
            Some(expected) => diesel::update(row.filter(dsl::group_hierarchy.eq(Some(expected))))
                .set(dsl::group_hierarchy.eq(Some(hierarchy)))
                .execute(conn)?,
            // SQL `NULL = NULL` is unknown, not true; spell the first
            // observation as `IS NULL` so a newly resolved row can match.
            None => diesel::update(row.filter(dsl::group_hierarchy.is_null()))
                .set(dsl::group_hierarchy.eq(Some(hierarchy)))
                .execute(conn)?,
        };
        if updated > 0 {
            cs.chats = true;
        }
    }
    Ok(())
}
