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
