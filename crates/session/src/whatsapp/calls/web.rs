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
    /// A `std` lock, like the desktop's, so the two answer the same way from
    /// the same threads — see the note on `CallRegistry::calls` there.
    pending: Arc<std::sync::Mutex<HashMap<String, Arc<WaIncomingCall>>>>,
}

impl CallRegistry {
    /// Record a ringing offer, so the conversation can say who called.
    pub(in crate::whatsapp) fn offer(&self, call_id: String, call: Arc<WaIncomingCall>) {
        self.pending
            .lock()
            .expect("call registry poisoned")
            .insert(call_id, call);
    }

    /// Forget a ringing offer, however it stopped ringing.
    pub(in crate::whatsapp) fn forget_offer(&self, call_id: &str) {
        self.pending
            .lock()
            .expect("call registry poisoned")
            .remove(call_id);
    }

    /// A call that ended without us, which here is only ever a ringing offer
    /// to forget: a page never answered one, so there is no local side and
    /// nothing in flight to leave a note for.
    pub(in crate::whatsapp) fn ended_remotely(&self, call_id: &str) {
        self.forget_offer(call_id);
    }

    /// Always false: a page opens no camera, so a call becoming live has none
    /// to make drawable and nothing to announce.
    pub(in crate::whatsapp) fn camera_became_drawable(&self, _call_id: &str) -> bool {
        false
    }
}

impl WhatsAppClient {
    /// Refused: answering a call needs the codec.
    ///
    /// With or without video, and the second is not a near miss: the picture
    /// would need an encoder as much as the voice needs one, and both are C.
    pub fn accept_call(&self, call_id: &str, _with_video: bool) {
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

    /// Refused: there is no live call here to put a camera on.
    ///
    /// `getUserMedia` would give a page the device, and `VideoEncoder` would
    /// give it H.264 — but neither is bound yet, and a call this side cannot
    /// answer has no direction to turn on in any case.
    pub fn set_call_video(&self, call_id: &str, _on: bool) {
        self.refuse_call(call_id, "shown on camera");
    }

    /// Nothing to ask: no camera here has ever encoded anything.
    ///
    /// Silent rather than refused, unlike everything above it. This is not a
    /// person asking for something — it is a window attaching mid-call and
    /// saying it has never seen a keyframe — so there is nothing to tell them
    /// they cannot have.
    pub fn request_video_keyframe(&self) {}

    /// Nothing to stop: a page never opened a camera to lose.
    ///
    /// Unused for that reason, and kept for the same one as everything else
    /// in [`crate::video`]'s browser half.
    ///
    /// Reached from the same place as on a desktop — the callback a camera
    /// reports its death through — and that callback is built here too,
    /// because building it is what `video::open` is handed. Since `open`
    /// always refuses, nothing ever calls this; it exists so the session's
    /// own code has one shape.
    #[allow(dead_code)]
    pub(in crate::whatsapp) async fn stop_local_video(
        _calls: &CallRegistry,
        _ui_sender: &UiEventSender,
        _call_id: &str,
        _only: Option<crate::video::CameraId>,
    ) {
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
