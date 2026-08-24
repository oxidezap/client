//! Call controls, one level above `client.voip()`.

use super::*;

impl WhatsAppApp {
    /// Get the current incoming call (if any)
    pub fn incoming_call(&self) -> Option<&IncomingCall> {
        self.call_state.incoming()
    }
    /// Get the current outgoing call (if any)
    pub fn outgoing_call(&self) -> Option<&OutgoingCall> {
        self.call_state.outgoing()
    }
    /// Accept the incoming call
    pub fn accept_call(&mut self, cx: &mut Context<Self>) {
        let Some(client) = &self.client else {
            warn!("Cannot accept call: client is unavailable");
            return;
        };
        if let Some(call) = self.call_state.take_incoming() {
            info!(
                "Accepting call {} from {}",
                call.call_id,
                observe_str(&call.caller_jid)
            );
            client.accept_call(call.call_id.as_str());
            cx.notify();
        }
    }
    /// Decline the incoming call
    pub fn decline_call(&mut self, cx: &mut Context<Self>) {
        let Some(client) = &self.client else {
            warn!("Cannot decline call: client is unavailable");
            return;
        };
        if let Some(call) = self.call_state.take_incoming() {
            info!(
                "Declining call {} from {}",
                call.call_id,
                observe_str(&call.caller_jid)
            );
            client.decline_call(call.call_id.as_str());
            cx.notify();
        }
    }
    /// Start a call to the specified JID
    pub fn start_call(&mut self, recipient_jid: String, is_video: bool, cx: &mut Context<Self>) {
        let Some(client) = &self.client else {
            warn!("Cannot start call: client is unavailable");
            return;
        };

        // Don't start a call if we already have an outgoing call
        if self.call_state.has_outgoing() {
            warn!("Already have an outgoing call in progress");
            return;
        }

        // Don't start a call if there's an incoming call
        if self.call_state.has_incoming() {
            warn!("Cannot start a call while there's an incoming call");
            return;
        }

        // Get the recipient name from the chat
        let recipient_name = self
            .find_chat(&recipient_jid)
            .map(|chat| chat.name.clone())
            .unwrap_or_else(|| "Unknown contact".to_string());

        info!(
            "Starting {} call to {}",
            if is_video { "video" } else { "audio" },
            observe_str(&recipient_jid)
        );

        // Create the outgoing call state
        let placeholder_call_id = format!("ui-call-{}", whatsapp_rust::wacore::time::now_millis());
        let call = OutgoingCall::new(
            placeholder_call_id.clone(),
            recipient_jid.clone(),
            recipient_name,
            is_video,
        );
        self.call_state.set_outgoing(call);

        // Initiate the call through the client
        client.start_call(&recipient_jid, is_video, placeholder_call_id);

        cx.notify();
    }
    /// Cancel the current outgoing call
    pub fn cancel_outgoing_call(&mut self, cx: &mut Context<Self>) {
        if let Some(call) = self.call_state.take_outgoing() {
            info!("Cancelling outgoing call {}", call.call_id);
            if let Some(client) = &self.client {
                client.cancel_call(call.call_id.as_str());
            }
            cx.notify();
        }
    }
}
