use whatsapp_rust::wacore_binary::JidExt as _;

use super::{Jid, WhatsAppClient};

impl WhatsAppClient {
    pub async fn schedule_message(
        &self,
        jid: &str,
        text: String,
        send_at_ms: i64,
    ) -> Result<(), String> {
        let jid = jid
            .parse::<Jid>()
            .map_err(|_| "invalid scheduled recipient")?;
        if !(jid.is_group() || jid.is_pn() || jid.is_lid()) {
            return Err("scheduled messages require a direct chat or group".into());
        }
        if text.trim().is_empty() || text.len() > 32 * 1024 {
            return Err("scheduled text must contain between 1 and 32,768 bytes".into());
        }
        let delay = send_at_ms.saturating_sub(wacore::time::now_millis());
        if !(1..=366 * 24 * 60 * 60 * 1000).contains(&delay) {
            return Err("scheduled time must be within the next year".into());
        }
        let live = self.session.lock().await.clone().ok_or("no session yet")?;
        if !live
            .chat_store
            .schedule_message(jid.to_non_ad_string(), text, send_at_ms)
            .await
            .map_err(|error| error.to_string())?
        {
            return Err("this account has 256 scheduled messages".into());
        }
        Ok(())
    }

    pub async fn next_scheduled_message(&self) -> Result<Option<i64>, String> {
        let live = self.session.lock().await.clone().ok_or("no session yet")?;
        live.chat_store
            .next_scheduled_message()
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn take_scheduled_message(&self) -> Result<Option<(String, String)>, String> {
        let live = self.session.lock().await.clone().ok_or("no session yet")?;
        live.chat_store
            .take_scheduled_message(wacore::time::now_millis())
            .await
            .map_err(|error| error.to_string())
    }
}

impl WhatsAppClient {
    pub fn scheduled_messages(
        &self,
        jid: String,
    ) -> super::Task<Result<Vec<oxidezap_core::ScheduledMessage>, String>> {
        let session = self.session.clone();
        self.exec.spawn(async move {
            let jid = jid.parse::<Jid>().map_err(|_| "invalid chat")?;
            let live = session.lock().await.clone().ok_or("no session yet")?;
            live.chat_store
                .scheduled_messages(jid.to_non_ad_string())
                .await
                .map_err(|error| error.to_string())
        })
    }

    pub async fn cancel_scheduled_message(&self, jid: String, id: i64) -> Result<(), String> {
        let jid = jid.parse::<Jid>().map_err(|_| "invalid chat")?;
        let live = self.session.lock().await.clone().ok_or("no session yet")?;
        if !live
            .chat_store
            .cancel_scheduled_message(jid.to_non_ad_string(), id)
            .await
            .map_err(|error| error.to_string())?
        {
            return Err("the message is no longer scheduled".into());
        }
        Ok(())
    }
}
