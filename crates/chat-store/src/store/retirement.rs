use diesel::prelude::*;
use diesel::sql_types::{Bool, Integer, Text};
use whatsapp_rust_sqlite_storage::SqliteStore;

use super::ChatStore;
use crate::error::{Result, db_err};

/// A durable redirect and retry record, independent of the retired device row.
#[derive(Clone, Debug, PartialEq, Eq, QueryableByName)]
pub struct AccountRetirement {
    #[diesel(sql_type = Integer)]
    pub old_id: i32,
    #[diesel(sql_type = Integer)]
    pub successor: i32,
    #[diesel(sql_type = Text)]
    pub pn: String,
    #[diesel(sql_type = Text)]
    pub lid: String,
    #[diesel(sql_type = Bool)]
    pub complete: bool,
}

impl ChatStore {
    pub async fn account_retirements(store: &SqliteStore) -> Result<Vec<AccountRetirement>> {
        Ok(store.shared().run(|conn| {
            diesel::sql_query("SELECT old_id, successor, pn, lid, complete FROM account_retirement ORDER BY old_id")
                .load(conn).map_err(db_err)
        }).await?)
    }

    /// Freeze an existing recovery edge and its exact persisted identity evidence.
    /// The caller must pin the successor lifecycle and verify full device loading.
    pub async fn begin_account_retirement(
        store: &SqliteStore,
        old_id: i32,
        successor: i32,
        old_pn: String,
        old_lid: String,
        pn: String,
        lid: String,
    ) -> Result<bool> {
        if old_id <= 0 || successor <= old_id {
            return Ok(false);
        }
        Ok(store.shared().run(move |conn| {
            conn.immediate_transaction::<_, diesel::result::Error, _>(|conn| {
                let count = diesel::sql_query(
                    "INSERT INTO account_retirement (old_id, successor, pn, lid) \
                     SELECT ?, ?, ?, ? WHERE \
                     EXISTS (SELECT 1 FROM session_recovery WHERE device_id = ? AND successor = ?) AND \
                     EXISTS (SELECT 1 FROM device WHERE id = ? AND pn = ? AND lid = ?) AND \
                     EXISTS (SELECT 1 FROM device WHERE id = ? AND pn = ? AND lid = ?) \
                     ON CONFLICT(old_id) DO NOTHING")
                    .bind::<Integer, _>(old_id).bind::<Integer, _>(successor)
                    .bind::<Text, _>(&pn).bind::<Text, _>(&lid)
                    .bind::<Integer, _>(old_id).bind::<Integer, _>(successor)
                    .bind::<Integer, _>(old_id).bind::<Text, _>(&old_pn).bind::<Text, _>(&old_lid)
                    .bind::<Integer, _>(successor).bind::<Text, _>(&pn).bind::<Text, _>(&lid)
                    .execute(conn)?;
                if count == 0 { return Ok(false); }
                // Do this before removing the intermediate row. Its FK otherwise
                // sets predecessors to NULL and loses the rest of a relink chain.
                diesel::sql_query("UPDATE session_recovery SET successor = ? WHERE successor = ? AND device_id < ?")
                    .bind::<Integer, _>(successor).bind::<Integer, _>(old_id).bind::<Integer, _>(old_id)
                    .execute(conn)?;
                Ok(true)
            }).map_err(db_err)
        }).await?)
    }

    pub async fn finish_account_retirement(store: &SqliteStore, old_id: i32) -> Result<()> {
        store
            .shared()
            .run(move |conn| {
                diesel::sql_query("UPDATE account_retirement SET complete = 1 WHERE old_id = ?")
                    .bind::<Integer, _>(old_id)
                    .execute(conn)
                    .map(|_| ())
                    .map_err(db_err)
            })
            .await?;
        Ok(())
    }
}
