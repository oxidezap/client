use diesel::prelude::*;
use diesel::sql_types::{BigInt, Integer, Text};

use super::ChatStore;
use crate::error::{Result, db_err};

impl ChatStore {
    pub async fn schedule_message(
        &self,
        jid: String,
        text: String,
        send_at_ms: i64,
    ) -> Result<bool> {
        let account = self.device_id();
        let inserted = self
            .db()
            .run(move |conn| {
                diesel::sql_query(
                    "INSERT INTO scheduled_messages (device_id, jid, text, send_at_ms)
                 SELECT ?1, ?2, ?3, ?4 WHERE (
                     SELECT COUNT(*) FROM scheduled_messages WHERE device_id = ?1
                 ) < 256",
                )
                .bind::<Integer, _>(account)
                .bind::<Text, _>(jid)
                .bind::<Text, _>(text)
                .bind::<BigInt, _>(send_at_ms)
                .execute(conn)
                .map_err(db_err)
            })
            .await?;
        Ok(inserted == 1)
    }

    pub async fn take_scheduled_message(&self, now_ms: i64) -> Result<Option<(String, String)>> {
        #[derive(QueryableByName)]
        struct ScheduledMessage {
            #[diesel(sql_type = Text)]
            jid: String,
            #[diesel(sql_type = Text)]
            text: String,
        }

        let account = self.device_id();
        let due = self
            .db()
            .run(move |conn| {
                diesel::sql_query(
                    "DELETE FROM scheduled_messages WHERE id = (
                     SELECT id FROM scheduled_messages WHERE device_id = ?1 AND send_at_ms <= ?2
                     ORDER BY send_at_ms, id LIMIT 1
                 ) RETURNING jid, text",
                )
                .bind::<Integer, _>(account)
                .bind::<BigInt, _>(now_ms)
                .get_result::<ScheduledMessage>(conn)
                .optional()
                .map_err(db_err)
            })
            .await?;
        Ok(due.map(|message| (message.jid, message.text)))
    }

    pub async fn next_scheduled_message(&self) -> Result<Option<i64>> {
        use diesel::sql_types::Nullable;

        #[derive(QueryableByName)]
        struct Deadline {
            #[diesel(sql_type = Nullable<BigInt>)]
            due: Option<i64>,
        }

        let account = self.device_id();
        let deadline = self
            .db()
            .read(move |conn| {
                diesel::sql_query(
                    "SELECT MIN(send_at_ms) AS due FROM scheduled_messages WHERE device_id = ?1",
                )
                .bind::<Integer, _>(account)
                .get_result::<Deadline>(conn)
                .map_err(db_err)
            })
            .await?;
        Ok(deadline.due)
    }

    pub async fn scheduled_messages(
        &self,
        jid: String,
    ) -> Result<Vec<oxidezap_core::ScheduledMessage>> {
        use crate::schema::scheduled_messages as scheduled;

        let account = self.device_id();
        let rows = self
            .db()
            .read(move |conn| {
                let keys = crate::lid::chat_key_candidates(conn, account, &jid).map_err(db_err)?;
                scheduled::table
                    .filter(scheduled::device_id.eq(account))
                    .filter(scheduled::jid.eq_any(keys))
                    .order((scheduled::send_at_ms.asc(), scheduled::id.asc()))
                    .select((
                        scheduled::id,
                        diesel::dsl::sql::<Text>("substr(text, 1, 120)"),
                        scheduled::send_at_ms,
                    ))
                    .load::<(i64, String, i64)>(conn)
                    .map_err(db_err)
            })
            .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, preview, send_at_ms)| oxidezap_core::ScheduledMessage {
                    id,
                    preview,
                    send_at_ms,
                },
            )
            .collect())
    }

    pub async fn cancel_scheduled_message(&self, jid: String, id: i64) -> Result<bool> {
        use crate::schema::scheduled_messages as scheduled;

        let account = self.device_id();
        let removed = self
            .db()
            .run(move |conn| {
                let keys = crate::lid::chat_key_candidates(conn, account, &jid).map_err(db_err)?;
                diesel::delete(
                    scheduled::table
                        .filter(scheduled::device_id.eq(account))
                        .filter(scheduled::id.eq(id))
                        .filter(scheduled::jid.eq_any(keys)),
                )
                .execute(conn)
                .map_err(db_err)
            })
            .await?;
        Ok(removed == 1)
    }
}
