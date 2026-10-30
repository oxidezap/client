//! Fixed first-row limit for the measured contact and identity lookups.
//!
//! SQLite 3.53 specializes a bound LIMIT on every step after rebinding.
//! These queries always need one row. A literal keeps limit-aware planning
//! without a runtime limit to specialize. See docs/sqlite-point-lookup-performance.md.
use diesel::prelude::*;
use diesel::query_builder::{AstPass, Query, QueryFragment, QueryId};
use diesel::sqlite::Sqlite;

#[derive(QueryId)]
pub(crate) struct First<Q>(pub(crate) Q);

impl<Q: Query> Query for First<Q> {
    type SqlType = Q::SqlType;
}

impl<Q: QueryFragment<Sqlite>> QueryFragment<Sqlite> for First<Q> {
    fn walk_ast<'b>(&'b self, mut out: AstPass<'_, 'b, Sqlite>) -> QueryResult<()> {
        self.0.walk_ast(out.reborrow())?;
        out.push_sql(" LIMIT 1");
        Ok(())
    }
}

impl<Q> RunQueryDsl<SqliteConnection> for First<Q> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::contacts::dsl;
    use diesel::connection::SimpleConnection;

    #[test]
    fn fixed_limit_keeps_filters_binds_and_optional_first_semantics() {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        conn.batch_execute("CREATE TABLE contacts(device_id INTEGER NOT NULL,jid TEXT NOT NULL,push_name TEXT,full_name TEXT,first_name TEXT,business_name TEXT,PRIMARY KEY(device_id,jid)); INSERT INTO contacts(device_id,jid) VALUES(1,'one'),(1,'two'),(2,'one')").unwrap();
        for (account, key) in [
            (1, "one"),
            (1, "two"),
            (2, "one"),
            (1, "absent"),
            (3, "one"),
        ] {
            let query = || {
                dsl::contacts
                    .filter(dsl::device_id.eq(account).and(dsl::jid.eq(key)))
                    .select(dsl::jid)
            };
            let expected = query().first::<String>(&mut conn).optional().unwrap();
            let fixed = First(query());
            let sql = diesel::debug_query::<Sqlite, _>(&fixed).to_string();
            assert!(sql.contains(" LIMIT 1 -- binds:"), "{sql}");
            assert!(!sql.contains("LIMIT ?"));
            assert_eq!(
                fixed.get_result::<String>(&mut conn).optional().unwrap(),
                expected
            );
        }
    }
}
