//! Calls in a browser: heard, declined, and not answered.
//!
//! Answering, placing, muting and turning a camera on all go through the
//! library's `voip` runtime, whose codec is C and does not build for
//! `wasm32-unknown-unknown`. So they are refused, and refused *here*, at the
//! method the front end calls, rather than deeper in where the failure would
//! arrive as a call that silently never connects.
//!
//! # Signalling is free of the feature; media is not
//!
//! `client.voip()` carries no `cfg`, and neither does `reject`: their stanza
//! builders live in `wacore`, so declining is a node on the socket that is
//! already open. What the `voip` feature gates is the media stack — accept,
//! call, mute, the relay and the engine — which is what pulls `tokio`'s `net`
//! and therefore mio's `compile_error!` on this target.
//!
//! This module used to conclude the opposite, from a real measurement of the
//! wrong thing: turning the feature on does fail exactly as described, but
//! `reject` never needed it. A decline reaches the caller from a page.
//!
//! `terminate` is ungated for the same reason and is unreachable here anyway:
//! a page never answers, so the registry holds ringing offers and nothing
//! that could be hung up.
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

    /// Take a ringing offer out to decline it.
    ///
    /// Removing and answering in one step, because the offer carries the
    /// identifiers `reject` needs and a second decline has nothing to send.
    pub(in crate::whatsapp) fn decline(&self, call_id: &str) -> Option<Arc<WaIncomingCall>> {
        self.pending
            .lock()
            .expect("call registry poisoned")
            .remove(call_id)
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

/// Why every call action here says no.
const CALLS_NEED_THE_DESKTOP: &str =
    "Calls need the desktop app: a browser has no audio codec for them.";

impl WhatsAppClient {
    /// Refused: answering a call needs the codec.
    ///
    /// With or without video, and the second is not a near miss: the picture
    /// would need an encoder as much as the voice needs one, and both are C.
    pub fn accept_call(&self, call_id: &str, _with_video: bool) {
        // `CallUnrecorded`, not the shared refusal. The daemon stages an
        // accept optimistically — `CallState::connect` before the backend is
        // asked — so by the time this runs the call is drawn as connecting in
        // every window. Ending it like an ordinary call would write a
        // zero-second call into the conversation for one that was never
        // answered, and the peer is still ringing meanwhile.
        warn!("a call cannot be answered in a browser: there is no audio codec here");
        let ui_sender = self.ui_sender.clone();
        let call_id = call_id.to_string();
        self.exec.spawn(async move {
            if let Some(tx) = ui_sender.lock().await.as_ref() {
                let _ = tx.send(UiEvent::CallUnrecorded(call_id));
            }
        });
    }

    /// Declined, and the caller is told so.
    ///
    /// The one call action a page performs rather than refuses: `reject` is a
    /// stanza builder in `wacore` behind no feature, so it needs none of the
    /// media stack that is missing here. The card is cleared whatever the
    /// send did, because the person has already answered the question the
    /// card was asking; the send is what stops the far end ringing.
    pub fn decline_call(&self, call_id: &str) {
        let client_handle = self.client_handle.clone();
        let calls = self.calls.clone();
        let ui_sender = self.ui_sender.clone();
        let call_id = call_id.to_string();

        self.exec.spawn(async move {
            let offer = calls.decline(&call_id);
            if let Some(tx) = ui_sender.lock().await.as_ref() {
                let _ = tx.send(UiEvent::CallEnded(call_id.clone()));
            }
            let Some(offer) = offer else {
                warn!("No pending offer for call {}", call_id);
                return;
            };
            let Some(client) = client_handle.lock().await.clone() else {
                error!("Client not available for declining call");
                return;
            };
            match client.voip().reject(&offer).await {
                Ok(()) => info!("Call {} declined", call_id),
                Err(e) => error!("Failed to decline call {}: {}", call_id, e),
            }
        });
    }

    /// Refused: placing a call needs the microphone and the codec.
    ///
    /// Answered with `OutgoingCallFailed` rather than the shared refusal,
    /// because that is the event for exactly this: it names the recipient, it
    /// carries the reason, and the daemon's `fail_outgoing_to` clears the
    /// stage and brings a parked caller forward. A `CallEnded` would say the
    /// call finished, which is a different thing from never having been
    /// placed — and the conversation records the two differently.
    pub fn start_call(&self, recipient_jid_str: &str, _is_video: bool, _placeholder_id: String) {
        warn!("a call cannot be placed in a browser: there is no audio codec here");
        let ui_sender = self.ui_sender.clone();
        let recipient_jid = recipient_jid_str.to_string();
        self.exec.spawn(async move {
            if let Some(tx) = ui_sender.lock().await.as_ref() {
                let _ = tx.send(UiEvent::OutgoingCallFailed {
                    recipient_jid,
                    error: CALLS_NEED_THE_DESKTOP.to_string(),
                });
            }
        });
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
    ///
    /// What it must **not** send is `UiEvent::Error`. The bridge translates
    /// that one variant into `ConnectionState::Disconnected` — it is the
    /// session's own "this is over" — so pressing Decline on a call the page
    /// cannot take used to leave the connected view, drop the chat list and
    /// start the reconnect path, over a WhatsApp session that was perfectly
    /// healthy. The refusal is about one call, and the only honest thing to
    /// end is that call.
    ///
    /// The reason therefore reaches the log and nowhere else, which is the
    /// same gap `AGENTS.md` records for a failed save: this app has no
    /// transient surface, and the only visible error state it has is the one
    /// that tears the session down.
    fn refuse_call(&self, call_id: &str, verb: &str) {
        warn!("a call cannot be {verb} in a browser: there is no audio codec here");
        let ui_sender = self.ui_sender.clone();
        let call_id = call_id.to_string();
        self.exec.spawn(async move {
            if let Some(tx) = ui_sender.lock().await.as_ref() {
                let _ = tx.send(UiEvent::CallEnded(call_id));
            }
        });
    }
}
