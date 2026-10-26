use diesel::prelude::*;
use diesel::sql_types::{Integer, Text};

use super::ChatStore;
use crate::error::{Result, db_err};

#[derive(QueryableByName)]
struct Logout {
    #[diesel(sql_type = Text)]
    logout_message: String,
}

impl ChatStore {
    /// The separate local account allocated for this account's next pairing.
    pub async fn pairing_successor(
        store: &whatsapp_rust_sqlite_storage::SqliteStore,
    ) -> Result<Option<i32>> {
        #[derive(QueryableByName)]
        struct Successor {
            #[diesel(sql_type = diesel::sql_types::Nullable<Integer>)]
            successor: Option<i32>,
        }
        let device_id = store.device_id();
        Ok(store
            .shared()
            .run(move |conn| {
                diesel::sql_query("SELECT successor FROM session_recovery WHERE device_id = ?")
                    .bind::<Integer, _>(device_id)
                    .get_result::<Successor>(conn)
                    .optional()
                    .map(|row| row.and_then(|row| row.successor))
                    .map_err(db_err)
            })
            .await?)
    }

    pub async fn record_pairing_successor(
        store: &whatsapp_rust_sqlite_storage::SqliteStore,
        successor: i32,
    ) -> Result<()> {
        let device_id = store.device_id();
        store.shared().run(move |conn| {
            diesel::sql_query("UPDATE session_recovery SET successor = ? WHERE device_id = ? AND successor IS NULL")
                .bind::<Integer, _>(successor)
                .bind::<Integer, _>(device_id)
                .execute(conn)
                .and_then(|count| if count == 1 { Ok(()) } else { Err(diesel::result::Error::NotFound) })
                .map_err(db_err)
        }).await?;
        Ok(())
    }

    /// The server's terminal rejection, retained with this account's history.
    pub async fn logout_message(&self) -> Result<Option<String>> {
        let device_id = self.device_id;
        Ok(self
            .db
            .run(move |conn| {
                diesel::sql_query("SELECT logout_message FROM session_recovery WHERE device_id = ?")
                    .bind::<Integer, _>(device_id)
                    .get_result::<Logout>(conn)
                    .optional()
                    .map(|row| row.map(|row| row.logout_message))
                    .map_err(db_err)
            })
            .await?)
    }

    /// Commit before publishing logout, so reopening cannot retry dead credentials.
    pub async fn record_logout(&self, message: String) -> Result<()> {
        let device_id = self.device_id;
        self.db.run(move |conn| {
            diesel::sql_query("INSERT INTO session_recovery (device_id, logout_message) VALUES (?, ?) ON CONFLICT(device_id) DO UPDATE SET logout_message = excluded.logout_message")
                .bind::<Integer, _>(device_id)
                .bind::<Text, _>(message)
                .execute(conn)
                .map(|_| ())
                .map_err(db_err)
        }).await?;
        Ok(())
    }
}
