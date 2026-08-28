//! Voice calls, which are the one thing the session does that a page cannot.
//!
//! Split out of `whatsapp.rs` because it is the only part of the session that
//! reaches for `whatsapp_rust::voip` — a stack whose codec is C and does not
//! build for `wasm32-unknown-unknown`. Keeping it in one file is what lets
//! the browser build leave it out without a `cfg` appearing anywhere in the
//! session's own logic.
//!
//! The methods are still `WhatsAppClient`'s; only the file changed. That is
//! the same split the GUI already uses for `app/`.

use super::super::*;
use oxidezap_audio::{spawn_mic, spawn_speaker};
use whatsapp_rust::voip::{CallHandle, CallTermination};

/// The two ends of a call's audio: what the microphone produces, and what the
/// speaker consumes.
type CallAudio = (
    async_channel::Receiver<Vec<i16>>,
    async_channel::Sender<Vec<i16>>,
);

/// Open the devices, off the async thread.
///
/// cpal's setup is blocking and can take a noticeable moment on a cold audio
/// stack, which is a moment the session would otherwise spend not answering
/// anything else.
async fn open_call_audio() -> Result<CallAudio, String> {
    tokio::task::spawn_blocking(|| {
        let mic = spawn_mic().map_err(|e| e.to_string())?;
        let speaker = spawn_speaker().map_err(|e| e.to_string())?;
        Ok((mic, speaker))
    })
    .await
    .map_err(|e| format!("audio setup task failed: {e}"))?
}

/// Live call state shared between the event pump and the UI action methods.
///
/// One lock over all of it, and that is the whole point. A call moves
/// *between* these collections — ringing to accepting to live, or to
/// cancelled — and every invariant worth having spans two of them at once.
/// With a mutex each, every transition was a check in one followed by an act
/// in another, and the gap between them was reachable: a `<terminate>`
/// landing there found the call in neither collection, recorded nothing, and
/// the acceptance went on to file a live handle behind the `CallEnded` the
/// window had already been sent. Narrowing that gap twice did not close it,
/// because it cannot be closed from outside the lock.
#[derive(Clone, Default)]
pub struct CallRegistry {
    calls: Arc<Mutex<Calls>>,
    /// Apart, deliberately. A mute lane is a `std::sync::Mutex` because a
    /// request is stamped on the caller's thread *before* its task exists —
    /// see [`MuteLane`] — and it takes no part in the ringing/live/cancelled
    /// invariants above.
    mute: Arc<std::sync::Mutex<HashMap<String, Arc<MuteLane>>>>,
}

/// What a cancel found to act on.
pub(in crate::whatsapp) enum Cancelled {
    /// A live call, handed back so the caller can terminate it.
    Live(Arc<CallHandle>),
    /// Nothing live yet, but a start is connecting and will honour this.
    Deferred,
    /// No such call.
    Nothing,
}

/// Everything one lock covers.
#[derive(Default)]
struct Calls {
    /// Ringing offers by call id, consumed by accept/decline.
    pending: HashMap<String, Arc<WaIncomingCall>>,
    /// Media-live calls by call id.
    active: HashMap<String, Arc<CallHandle>>,
    /// Ids ended before any handle existed — the UI's placeholder id while
    /// `start_call` is still connecting, or a peer's `<terminate>` arriving
    /// while `accept_call` is opening the microphone. Whichever call is in
    /// flight hangs these up on arrival.
    cancelled: HashSet<String>,
    /// Acceptances in flight: the offer has left `pending` and no handle
    /// exists yet. Opening the audio devices and connecting the relay both
    /// take time, and an ending arriving in that window has nowhere else to
    /// be written down.
    in_flight: HashSet<String>,
}

impl CallRegistry {
    /// Record a ringing offer, so accept and decline have something to act on.
    pub(in crate::whatsapp) async fn offer(&self, call_id: String, call: Arc<WaIncomingCall>) {
        self.calls.lock().await.pending.insert(call_id, call);
    }

    /// Forget a ringing offer, however it stopped ringing.
    pub(in crate::whatsapp) async fn forget_offer(&self, call_id: &str) {
        self.calls.lock().await.pending.remove(call_id);
    }

    /// Take a ringing offer for something that is *not* an acceptance.
    ///
    /// A decline needs the offer to reject it, and nothing about it is in
    /// flight afterwards — so unlike [`Self::begin_accept`] there is no
    /// window here for an ending to fall into.
    pub(in crate::whatsapp) async fn forget_and_take_offer(
        &self,
        call_id: &str,
    ) -> Option<Arc<WaIncomingCall>> {
        self.calls.lock().await.pending.remove(call_id)
    }

    /// Take a ringing offer and mark its acceptance as in flight, together.
    ///
    /// One operation because they are one step: between leaving `pending` and
    /// entering `in_flight` the call would be in nothing at all, which is the
    /// state that has no answer for a peer hanging up.
    pub(in crate::whatsapp) async fn begin_accept(
        &self,
        call_id: &str,
    ) -> Option<Arc<WaIncomingCall>> {
        let mut calls = self.calls.lock().await;
        let offer = calls.pending.remove(call_id)?;
        calls.in_flight.insert(call_id.to_string());
        Some(offer)
    }

    /// File an accepted call as live — unless the peer ended it meanwhile.
    ///
    /// `false` means the window has already been told the call is over, so
    /// the handle is not filed and the caller hangs it up locally. Both
    /// answers leave `in_flight` empty for this id.
    pub(in crate::whatsapp) async fn finish_accept(
        &self,
        call_id: &str,
        handle: &Arc<CallHandle>,
    ) -> bool {
        let mut calls = self.calls.lock().await;
        calls.in_flight.remove(call_id);
        if calls.cancelled.remove(call_id) {
            return false;
        }
        calls.active.insert(call_id.to_string(), Arc::clone(handle));
        true
    }

    /// An acceptance that produced no handle. Says whether the peer had
    /// already ended it, so a caller knows whether anyone is still ringing.
    pub(in crate::whatsapp) async fn abandon_accept(&self, call_id: &str) -> bool {
        let mut calls = self.calls.lock().await;
        calls.in_flight.remove(call_id);
        calls.cancelled.remove(call_id)
    }

    /// Mark an outgoing call as being placed, under the id the window drew.
    pub(in crate::whatsapp) async fn begin_start(&self, placeholder: &str) {
        self.calls
            .lock()
            .await
            .in_flight
            .insert(placeholder.to_string());
    }

    /// File a placed call as live under its real id — unless it was cancelled
    /// while connecting, in which case the caller terminates it.
    ///
    /// The placeholder is what a cancel names, because it is the only id the
    /// window had; the rename and the cancellation are answered together so a
    /// cancel arriving between them cannot be lost.
    pub(in crate::whatsapp) async fn finish_start(
        &self,
        placeholder: &str,
        call_id: &str,
        handle: &Arc<CallHandle>,
    ) -> bool {
        let mut calls = self.calls.lock().await;
        calls.in_flight.remove(placeholder);
        // Either name: the window cancels under the placeholder, and anything
        // that learned the real id first cancels under that.
        let cancelled = calls.cancelled.remove(placeholder) | calls.cancelled.remove(call_id);
        if cancelled {
            return false;
        }
        calls.active.insert(call_id.to_string(), Arc::clone(handle));
        true
    }

    /// An outgoing call that never produced a handle.
    pub(in crate::whatsapp) async fn abandon_start(&self, placeholder: &str) {
        let mut calls = self.calls.lock().await;
        calls.in_flight.remove(placeholder);
        calls.cancelled.remove(placeholder);
    }

    /// The peer ended this call: drop the local side without answering.
    ///
    /// `hangup_local`, not `terminate`: they are the side that ended it, and
    /// answering their `<terminate>` with one of our own says nothing they do
    /// not already know. Only the local media task and the registry entry are
    /// left to drop.
    ///
    /// Done here rather than by handing the handle back, so that a caller
    /// never has to name a `CallHandle` — the type is the media stack's, and
    /// the media stack is what a browser does not have.
    ///
    /// A call with no handle *yet* is the case the second half is for, and it
    /// is decided under the same lock the handle would be filed under: an
    /// acceptance in flight will produce one after this returns, so the news
    /// is left where that acceptance is guaranteed to look for it.
    pub(in crate::whatsapp) async fn ended_by_peer(&self, call_id: &str) {
        let mut calls = self.calls.lock().await;
        if let Some(handle) = calls.active.remove(call_id) {
            tokio::spawn(async move { handle.hangup_local().await });
            return;
        }
        if calls.in_flight.contains(call_id) {
            calls.cancelled.insert(call_id.to_string());
        }
    }

    /// Take a live call out, for a caller that is going to end it itself.
    pub(in crate::whatsapp) async fn take_live(&self, call_id: &str) -> Option<Arc<CallHandle>> {
        self.calls.lock().await.active.remove(call_id)
    }

    /// Ask for a live call without taking it.
    pub(in crate::whatsapp) async fn live(&self, call_id: &str) -> Option<Arc<CallHandle>> {
        self.calls.lock().await.active.get(call_id).cloned()
    }

    /// Cancel a call under whichever name the caller has for it.
    ///
    /// One operation, because the three answers are decided by the same
    /// state: a live handle is taken and returned for the caller to
    /// terminate; a call still connecting has the cancel written where
    /// [`Self::finish_start`] will find it; anything else never existed. Done
    /// separately, "no live handle" and "leave a note" were two lock
    /// acquisitions with a gap between them — and a start filing its handle
    /// in that gap consumed the note before it was written, so the abandoned
    /// attempt rang on at the far end.
    ///
    /// A note is only left where something is in flight to receive it, or the
    /// set would grow by one entry for every call that merely rang and
    /// stopped.
    pub(in crate::whatsapp) async fn cancel(&self, call_id: &str) -> Cancelled {
        let mut calls = self.calls.lock().await;
        calls.pending.remove(call_id);
        if let Some(handle) = calls.active.remove(call_id) {
            return Cancelled::Live(handle);
        }
        if calls.in_flight.contains(call_id) {
            calls.cancelled.insert(call_id.to_string());
            return Cancelled::Deferred;
        }
        Cancelled::Nothing
    }

    /// Mark an acceptance in flight without an offer, for tests.
    ///
    /// [`Self::begin_accept`] is the real entry and takes a `WaIncomingCall`,
    /// which is `#[non_exhaustive]` upstream and so cannot be built here at
    /// all. What these tests are about is where a call *is* — the transition
    /// between in-flight and live-or-cancelled — rather than what its offer
    /// said, and this reaches that state directly.
    #[cfg(test)]
    pub(in crate::whatsapp) async fn mark_accepting(&self, call_id: &str) {
        self.calls
            .lock()
            .await
            .in_flight
            .insert(call_id.to_string());
    }

    /// Every id that currently names a live call, for pruning the mute lanes.
    pub(in crate::whatsapp) async fn live_ids(&self) -> HashSet<String> {
        self.calls.lock().await.active.keys().cloned().collect()
    }
}

/// What keeps a call's mute requests in the order the daemon took them.
///
/// Spawning is not sequencing: two requests spawned in order can start in
/// either one, and the last to reach the wire wins. That is how a rapid
/// unmute-then-mute could leave the microphone open under a state — and every
/// window — showing it muted, with both tasks finding the device in the state
/// they themselves had asked for and so correcting nothing.
#[derive(Default)]
struct MuteLane {
    /// The newest request, stamped on the caller's thread *before* its task
    /// exists. That is the only place the order still exists.
    ///
    /// A `std` lock on purpose: it is taken from a synchronous method and
    /// never held across an await.
    intent: std::sync::Mutex<MuteIntent>,
    /// One announcement in flight per call. The library serializes its own
    /// transitions, but it serializes them in arrival order, which is the
    /// order this exists to stop trusting.
    lane: Mutex<()>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct MuteIntent {
    /// Bumped per request, so a task can ask whether it is still the newest.
    seq: u64,
    muted: bool,
}

impl WhatsAppClient {
    /// Accept an incoming call: signaling, callKey decrypt, relay connect and
    /// the audio engine are all inside `client.voip().accept(..)`; this side
    /// only supplies the cpal mic/speaker bridge.
    pub fn accept_call(&self, call_id: &str) {
        let client_handle = self.client_handle.clone();
        let calls = self.calls.clone();
        let ui_sender = self.ui_sender.clone();
        let call_id = call_id.to_string();

        self.exec.spawn(async move {
            let Some(client) = client_handle.lock().await.clone() else {
                error!("Client not available for accepting call");
                return;
            };
            // Taken and marked in flight together: between leaving `pending`
            // and being marked, the call would be in nothing at all, and a
            // `<terminate>` landing there has nowhere to be written down.
            let Some(offer) = calls.begin_accept(&call_id).await else {
                warn!("No pending offer for call {}", call_id);
                return;
            };
            let (mic, speaker) = match open_call_audio().await {
                Ok(audio) => audio,
                Err(err) => {
                    error!("Audio device setup failed: {err}");
                    // Nothing was accepted, so there is nothing for a peer's
                    // ending to race any more.
                    let ended_by_peer = calls.abandon_accept(&call_id).await;
                    // The offer is consumed and no accept went out: reject
                    // so the caller stops ringing instead of waiting out
                    // the timeout. Unless they hung up first, in which case
                    // there is nobody left ringing to tell.
                    if !ended_by_peer && let Err(e) = client.voip().reject(&offer).await {
                        error!(
                            "Failed to reject call {} after audio failure: {}",
                            call_id, e
                        );
                    }
                    Self::notify_call_ended(&ui_sender, &call_id).await;
                    return;
                }
            };
            match client
                .voip()
                .accept(&offer)
                .audio(mic, speaker)
                .start()
                .await
            {
                Ok(handle) => {
                    let handle = Arc::new(handle);
                    // Filed, or refused, under the one lock: there is no
                    // moment where this call is neither in flight nor live.
                    if !calls.finish_accept(&call_id, &handle).await {
                        // The peer hung up while this was connecting, and the
                        // window has already drawn the call as over. Filing
                        // the handle as live would leave a microphone open
                        // under a conversation nobody thinks is happening,
                        // and a second `CallEnded` behind it when the watcher
                        // eventually fired.
                        //
                        // `hangup_local`, because they are the side that
                        // ended it: their `<terminate>` is already out and
                        // one of ours would say nothing new.
                        info!("Call {} was ended by the peer while connecting", call_id);
                        handle.hangup_local().await;
                        return;
                    }
                    info!("Call {} media live", handle.call_id());
                    Self::watch_call_end(handle, calls.clone(), ui_sender.clone());
                }
                Err(e) => {
                    error!("Failed to start call media for {}: {}", call_id, e);
                    calls.abandon_accept(&call_id).await;
                    Self::notify_call_ended(&ui_sender, &call_id).await;
                }
            }
        });
    }

    /// Decline an incoming call (sends the reject signaling).
    pub fn decline_call(&self, call_id: &str) {
        let client_handle = self.client_handle.clone();
        let calls = self.calls.clone();
        let call_id = call_id.to_string();

        self.exec.spawn(async move {
            let Some(client) = client_handle.lock().await.clone() else {
                error!("Client not available for declining call");
                return;
            };
            let Some(offer) = calls.forget_and_take_offer(&call_id).await else {
                warn!("No pending offer for call {}", call_id);
                return;
            };
            match client.voip().reject(&offer).await {
                Ok(()) => info!("Call {} declined", call_id),
                Err(e) => error!("Failed to decline call {}: {}", call_id, e),
            }
        });
    }

    /// Place an outgoing 1:1 voice call. Device discovery, callKey encrypt,
    /// offer send and the relay/engine lifecycle are inside
    /// `client.voip().call(..)`. Video calls are not supported by the library
    /// yet; `is_video` only shapes the UI.
    pub fn start_call(&self, recipient_jid_str: &str, is_video: bool, placeholder_id: String) {
        let client_handle = self.client_handle.clone();
        let calls = self.calls.clone();
        let ui_sender = self.ui_sender.clone();
        let recipient_jid = recipient_jid_str.to_string();

        if is_video {
            warn!("Video calls are not supported yet; placing a voice call");
        }

        self.exec.spawn(async move {
            // Before the first await: a cancel arriving after this has
            // somewhere to be written down, and one arriving before it finds
            // nothing — which is right, because nothing has been placed.
            calls.begin_start(&placeholder_id).await;
            let notify_failure = |error: String| {
                let ui_sender = ui_sender.clone();
                let recipient_jid = recipient_jid.clone();
                // A cancel may have landed for a call that will never
                // start; consume the marker so the set doesn't grow.
                let calls = calls.clone();
                let placeholder_id = placeholder_id.clone();
                async move {
                    calls.abandon_start(&placeholder_id).await;
                    error!(
                        "Failed to start call to {}: {}",
                        observe_str(&recipient_jid),
                        error
                    );
                    if let Some(tx) = ui_sender.lock().await.as_ref() {
                        let _ = tx.send(UiEvent::OutgoingCallFailed {
                            recipient_jid,
                            error,
                        });
                    }
                }
            };

            let jid: Jid = match recipient_jid.parse() {
                Ok(j) => j,
                Err(e) => {
                    notify_failure(format!("invalid JID: {e}")).await;
                    return;
                }
            };
            let Some(client) = client_handle.lock().await.clone() else {
                notify_failure("client not available".to_string()).await;
                return;
            };
            let (mic, speaker) = match open_call_audio().await {
                Ok(audio) => audio,
                Err(err) => {
                    notify_failure(format!("audio device setup failed: {err}")).await;
                    return;
                }
            };

            match client.voip().call(&jid).audio(mic, speaker).start().await {
                Ok(handle) => {
                    let call_id = handle.call_id().to_string();
                    let handle = Arc::new(handle);
                    // The rename and the cancellation are one decision. The
                    // window only ever knew the placeholder, so a cancel
                    // names that — and answering it separately from filing
                    // the handle left a gap where the cancel was consumed
                    // before it was written and the call rang on.
                    if !calls.finish_start(&placeholder_id, &call_id, &handle).await {
                        info!("Outgoing call {} cancelled before start", call_id);
                        // The offer is already out: every device it rang is
                        // ringing, and dropping our side silently would leave
                        // them at it until their own transport gave up.
                        // `terminate` is what tells them, and it tears this
                        // side down whether or not the stanzas landed.
                        log_termination(&call_id, handle.terminate().await);
                        return;
                    }
                    info!(
                        "Outgoing call {} to {} offered",
                        call_id,
                        observe_str(&recipient_jid)
                    );
                    Self::watch_call_end(handle, calls.clone(), ui_sender.clone());
                    if let Some(tx) = ui_sender.lock().await.as_ref() {
                        let _ = tx.send(UiEvent::OutgoingCallStarted {
                            call_id,
                            recipient_jid,
                            placeholder_id,
                        });
                    }
                }
                Err(e) => notify_failure(e.to_string()).await,
            }
        });
    }

    /// Hang up / cancel a call we started or answered.
    pub fn cancel_call(&self, call_id: &str) {
        let calls = self.calls.clone();
        let call_id = call_id.to_string();

        self.exec.spawn(async move {
            match calls.cancel(&call_id).await {
                Cancelled::Live(handle) => {
                    log_termination(&call_id, handle.terminate().await);
                }
                Cancelled::Deferred => {
                    // `start_call` is still connecting under the placeholder
                    // id the window drew; it honours this on arrival.
                    debug!("cancel_call: no live handle for {}, deferring", call_id);
                }
                Cancelled::Nothing => {
                    debug!("cancel_call: nothing to cancel for {}", call_id);
                }
            }
        });
    }

    /// Mute or unmute the microphone of a live call, and tell the peer.
    ///
    /// The library commits the two directions around the `<mute_v2>` rather
    /// than at one point — a mute applies before the announcement, an unmute
    /// only once it is out — so whichever half is lost, the microphone is
    /// never live while the peer is being shown a muted one. What that costs
    /// is that a failed announcement leaves the device in a state nobody
    /// asked for, and the front end has already drawn the state it asked for.
    /// So the handle is asked what it really holds and the answer is
    /// published — always, not only when it differs: what makes the state
    /// trustworthy is that the *last* request to reach the device is the one
    /// that speaks last, and a task that only spoke on disagreement would
    /// leave a failed announcement's answer standing over a later success.
    /// It costs nothing, because a call state that does not change sends no
    /// frame.
    ///
    /// The request is stamped here, on the caller's thread, and the work is
    /// what gets spawned — see [`MuteLane`]. A task compares the device
    /// against the *newest* request rather than its own, because its own is
    /// exactly what a superseded task must not restore.
    ///
    /// A call still ringing has nowhere to publish the state, and answering
    /// does not replay it. That is not a gap here: mute is offered on an
    /// active call only ([`oxidezap_core::CallState::set_muted`] matches the
    /// live stage), so nothing can be chosen while it rings.
    pub fn set_call_muted(&self, call_id: &str, muted: bool) {
        let calls = self.calls.clone();
        let ui_sender = self.ui_sender.clone();
        let call_id = call_id.to_string();

        // Before the spawn, because after it the order is gone.
        let (lane, seq) = {
            let mut lanes = calls.mute.lock().expect("mute lanes poisoned");
            let lane = lanes.entry(call_id.clone()).or_default().clone();
            let mut intent = lane.intent.lock().expect("mute intent poisoned");
            intent.seq += 1;
            intent.muted = muted;
            let seq = intent.seq;
            drop(intent);
            (lane, seq)
        };

        self.exec.spawn(async move {
            // Cloned out from under the lock: `set_muted` waits on the call's
            // answer-transition lane, and holding the registry across that
            // would stall every other call's bookkeeping behind one peer.
            let handle = {
                let live = calls.live_ids().await;
                // Where the lane map grows is where it is swept.
                calls
                    .mute
                    .lock()
                    .expect("mute lanes poisoned")
                    .retain(|id, _| live.contains(id));
                calls.live(&call_id).await
            };
            let Some(handle) = handle else {
                debug!("set_call_muted: no live handle for {}", call_id);
                return;
            };

            let _serialized = lane.lane.lock().await;
            // A newer request either has already run or is blocked on the
            // lane behind us; either way it, and not this one, is what the
            // device should end up saying.
            let want = *lane.intent.lock().expect("mute intent poisoned");
            if want.seq != seq {
                return;
            }
            if let Err(e) = handle.set_muted(want.muted).await {
                warn!(
                    "Failed to announce {} on call {}: {}",
                    if want.muted { "mute" } else { "unmute" },
                    call_id,
                    e
                );
            }
            // Superseded while announcing: the request behind us is about to
            // set the state anyway, and a word from here would describe a
            // device that is already on its way somewhere else. It speaks
            // after it has arrived, which is what makes it the last word.
            if lane.intent.lock().expect("mute intent poisoned").seq != seq {
                return;
            }
            // Said whether or not it is news, and this is why. A correction
            // sent only on disagreement is unversioned, and the daemon writes
            // a request's optimistic state before that request is even
            // stamped here — so a *failed* announcement could publish its
            // truth into the window belonging to the retry queued behind it,
            // and the retry, succeeding, would find agreement and say
            // nothing. The state would then hold the failure's answer over
            // the success's device. Speaking unconditionally makes the newest
            // request the one that closes the exchange, and costs nothing:
            // the daemon publishes no frame for a state that did not change.
            let settled = handle.is_muted();
            if let Some(tx) = ui_sender.lock().await.as_ref() {
                let _ = tx.send(UiEvent::CallMuteChanged {
                    call_id,
                    muted: settled,
                });
            }
        });
    }

    /// Watch a live call until it ends (peer hangup, network loss, local
    /// hangup) and clear it from the registry + UI.
    fn watch_call_end(handle: Arc<CallHandle>, calls: CallRegistry, ui_sender: UiEventSender) {
        tokio::spawn(async move {
            handle.wait_ended().await;
            let call_id = handle.call_id().to_string();
            calls.take_live(&call_id).await;
            // Every call that ever had a handle drains through here, whatever
            // ended it, so this is where a lane is paid for. The sweep in
            // `set_call_muted` is not made redundant by it: a window that fell
            // behind can stamp a request against a call this watcher has
            // already run for, and that lane has no second ending to be
            // removed on.
            calls
                .mute
                .lock()
                .expect("mute lanes poisoned")
                .remove(&call_id);
            Self::notify_call_ended(&ui_sender, &call_id).await;
        });
    }

    async fn notify_call_ended(ui_sender: &UiEventSender, call_id: &str) {
        if let Some(tx) = ui_sender.lock().await.as_ref() {
            let _ = tx.send(UiEvent::CallEnded(call_id.to_string()));
        }
    }
}

/// Say what a hangup achieved.
///
/// The local side is down in every case, so this reports rather than fails: a
/// call the peer was never told about is still over here, and the difference
/// is only how long they keep ringing. A still-ringing call is addressed per
/// device, which is why "some, not all" is one of the answers.
fn log_termination(call_id: &str, outcome: CallTermination) {
    match outcome {
        CallTermination::PeerNotified => info!("Call {} hung up", call_id),
        CallTermination::PartlyNotified {
            notified,
            unconfirmed,
        } => warn!(
            "Call {} hung up; {} device(s) told, {} unconfirmed",
            call_id, notified, unconfirmed
        ),
        CallTermination::LocalOnly(error) => warn!(
            "Call {} hung up locally; the peer was not told: {}",
            call_id, error
        ),
        CallTermination::AlreadyEnded => debug!("Call {} was already over", call_id),
        // `CallTermination` is `#[non_exhaustive]`: a variant added upstream
        // is still an ended call here, and the local side is down in every
        // one of them.
        other => info!("Call {} hung up: {:?}", call_id, other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stamp a request the way `set_call_muted` does, on the caller's thread.
    fn request(lane: &MuteLane, muted: bool) -> u64 {
        let mut intent = lane.intent.lock().unwrap();
        intent.seq += 1;
        intent.muted = muted;
        intent.seq
    }

    /// Two toggles in quick succession are spawned as two tasks, and spawn
    /// order is not run order. Run the wrong way round, each task saw the
    /// device holding the value it had itself asked for and corrected
    /// nothing — so an unmute that executed last left the microphone open
    /// under a state, and every window, still showing it muted.
    ///
    /// The order survives because it is stamped before the tasks exist, and a
    /// task that is no longer the newest does nothing at all.
    #[test]
    fn only_the_newest_mute_request_reaches_the_device() {
        let lane = MuteLane::default();
        // Muted, and the user changes their mind twice.
        let unmute = request(&lane, false);
        let remute = request(&lane, true);

        // Whichever task wins the lane, the gate answers the same way.
        let newest = *lane.intent.lock().unwrap();
        assert_ne!(unmute, remute);
        assert_eq!(newest.seq, remute, "the last request is the live one");
        assert!(newest.muted, "and it is the one the device must end on");
        assert_ne!(
            newest.seq, unmute,
            "the superseded task yields instead of restoring its own value"
        );
    }

    /// The window between an offer leaving `pending` and a handle reaching
    /// `active` is a real one — opening the audio devices and connecting the
    /// relay both take time — and a `<terminate>` landing inside it used to
    /// find the call in neither collection, remove nothing, and let the
    /// acceptance file a live handle behind the `CallEnded` the window had
    /// already been sent. A microphone open under a call nobody thought was
    /// happening.
    #[tokio::test]
    async fn a_peer_ending_a_call_mid_acceptance_is_not_lost() {
        let calls = CallRegistry::default();
        calls.mark_accepting("call-1").await;
        calls.ended_by_peer("call-1").await;

        assert!(
            calls.abandon_accept("call-1").await,
            "an ending that arrived mid-acceptance must reach the acceptance"
        );
    }

    /// And it is spent once, so a later acceptance of a call with a reused id
    /// is not cancelled by a stale note.
    #[tokio::test]
    async fn a_peer_ending_is_reported_once() {
        let calls = CallRegistry::default();
        calls.mark_accepting("call-1").await;
        calls.ended_by_peer("call-1").await;

        assert!(calls.abandon_accept("call-1").await);
        assert!(
            !calls.abandon_accept("call-1").await,
            "the note is consumed by the acceptance that acted on it"
        );
    }

    /// Nothing is recorded when no acceptance is in flight, or the set would
    /// grow by one entry for every call that simply rang and stopped.
    #[tokio::test]
    async fn an_ending_with_nothing_in_flight_records_nothing() {
        let calls = CallRegistry::default();
        calls.ended_by_peer("call-1").await;

        calls.mark_accepting("call-1").await;
        assert!(
            !calls.abandon_accept("call-1").await,
            "an ending before the acceptance began is not this acceptance's"
        );
    }

    /// A cancel for a call that is still connecting has to survive until the
    /// handle exists.
    ///
    /// The window only ever knew the placeholder id, so that is what it
    /// cancels under. Answering the cancel separately from filing the handle
    /// left a gap where a start consumed the note before it was written —
    /// and the abandoned attempt then rang at the far end until its transport
    /// gave up.
    #[tokio::test]
    async fn a_cancel_while_connecting_reaches_the_start() {
        let calls = CallRegistry::default();
        calls.begin_start("placeholder-1").await;

        assert!(
            matches!(calls.cancel("placeholder-1").await, Cancelled::Deferred),
            "nothing is live yet, so the cancel is left for the start"
        );
    }

    /// And a cancel with nothing in flight is not remembered, for the same
    /// reason an ending is not.
    #[tokio::test]
    async fn a_cancel_with_nothing_in_flight_is_not_remembered() {
        let calls = CallRegistry::default();
        assert!(matches!(
            calls.cancel("placeholder-1").await,
            Cancelled::Nothing
        ));
    }

    /// A lone request is nobody's stale task    /// A lone request is nobody's stale task: it applies, and it is the one
    /// that answers for what the device really did.
    #[test]
    fn a_single_mute_request_is_the_newest_one() {
        let lane = MuteLane::default();
        let seq = request(&lane, true);
        let newest = *lane.intent.lock().unwrap();
        assert_eq!(newest.seq, seq);
        assert!(newest.muted);
    }
}
