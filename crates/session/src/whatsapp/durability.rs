use std::sync::Arc;

use async_trait::async_trait;
use oxidezap_chat_store::ChatStore;
use whatsapp_rust::InboundDurabilityHook;
use whatsapp_rust::client::Client;
use whatsapp_rust::types::durability_hook::InboundMessage;

pub(super) struct ChatStoreDurabilityHook {
    chat_store: Arc<ChatStore>,
}

impl ChatStoreDurabilityHook {
    pub(super) fn new(chat_store: Arc<ChatStore>) -> Self {
        Self { chat_store }
    }
}

#[async_trait]
impl InboundDurabilityHook for ChatStoreDurabilityHook {
    async fn on_messages(
        &self,
        _client: Arc<Client>,
        batch: &[InboundMessage],
    ) -> whatsapp_rust::anyhow::Result<()> {
        self.chat_store
            .commit_inbound_batch(batch)
            .await
            .map_err(|error| whatsapp_rust::anyhow::Error::msg(error.to_string()))
    }
}
