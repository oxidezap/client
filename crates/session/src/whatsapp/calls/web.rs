//! Calls in a browser: heard, recorded, and not answered.
//!
//! Every one of the five actions goes through `client.voip()`, and that is
//! the module a page does not have — libopus and the RTC stack are C. So the
//! actions are refused, and refused *here*, at the method the front end
//! calls, rather than deeper in where the failure would arrive as a call that
//! silently never connects.
//!
//! What is kept is the ringing. A page learns about an incoming call like it
//! learns about anything else, so the offer is recorded and forgotten on the
//! same events as on the desktop — which is what leaves an honest missed-call
//! record in the conversation instead of a gap.

use super::super::*;

/// Ringing offers, and nothing else.
///
/// The desktop registry also holds live calls and a mute lane per call. A
/// page has neither, because it never gets as far as a live call.
#[derive(Clone, Default)]
pub struct CallRegistry {
    pending: Arc<Mutex<HashMap<String, Arc<WaIncomingCall>>>>,
}

impl CallRegistry {
    /// Record a ringing offer, so the conversation can say who called.
    pub(in crate::whatsapp) async fn offer(&self, call_id: String, call: Arc<WaIncomingCall>) {
        self.pending.lock().await.insert(call_id, call);
    }

    /// Forget a ringing offer, however it stopped ringing.
    pub(in crate::whatsapp) async fn forget_offer(&self, call_id: &str) {
        self.pending.lock().await.remove(call_id);
    }

    /// Nothing to end: a call that was never answered has no local side.
    ///
    /// Present so the event pump reads the same on both platforms — the peer
    /// hanging up is the same event here, and it is the removal above that
    /// does the work.
    pub(in crate::whatsapp) async fn ended_by_peer(&self, _call_id: &str) {}
}

impl WhatsAppClient {
    /// Refused: answering a call needs the codec.
    pub fn accept_call(&self, call_id: &str) {
        self.refuse_call(call_id, "answered");
    }

    /// Refused for the same reason as accepting.
    ///
    /// A decline is not just a local dismissal — it tells the caller to stop
    /// ringing, and that goes through `client.voip().reject`. Doing nothing
    /// but clearing it here would leave the other side ringing until their
    /// own timeout, which is worse than saying the window cannot do it.
    pub fn decline_call(&self, call_id: &str) {
        self.refuse_call(call_id, "declined");
    }

    /// Refused: placing a call needs the microphone and the codec.
    pub fn start_call(&self, _recipient_jid_str: &str, _is_video: bool, placeholder_id: String) {
        self.refuse_call(&placeholder_id, "placed");
    }

    /// Refused, and harmless: nothing was ever placed to cancel.
    pub fn cancel_call(&self, call_id: &str) {
        self.refuse_call(call_id, "cancelled");
    }

    /// Refused: there is no live call here to mute.
    pub fn set_call_muted(&self, call_id: &str, _muted: bool) {
        self.refuse_call(call_id, "muted");
    }

    /// Say no, once, in the terms the caller understands.
    ///
    /// A `CallEnded` as well as the log, because the front end draws a call
    /// card the moment it asks and watches the state for it to go away. A
    /// refusal that only logged would leave that card up with nothing behind
    /// it and no way to dismiss it.
    fn refuse_call(&self, call_id: &str, verb: &str) {
        warn!("a call cannot be {verb} in a browser: there is no audio codec here");
        let ui_sender = self.ui_sender.clone();
        let call_id = call_id.to_string();
        self.exec.spawn(async move {
            if let Some(tx) = ui_sender.lock().await.as_ref() {
                let _ = tx.send(UiEvent::Error(
                    "Calls need the desktop app: a browser has no audio codec for them."
                        .to_string(),
                ));
                let _ = tx.send(UiEvent::CallEnded(call_id));
            }
        });
    }
}
