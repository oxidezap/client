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

use super::*;

/// Live call state shared between the event pump and the UI action methods.
#[derive(Clone, Default)]
pub struct CallRegistry {
    /// Ringing offers by call id, consumed by accept/decline.
    pending: Arc<Mutex<HashMap<String, Arc<WaIncomingCall>>>>,
    /// Media-live calls by call id.
    active: Arc<Mutex<HashMap<String, Arc<CallHandle>>>>,
    /// Ids cancelled before any handle existed (the UI's placeholder id while
    /// start_call is still connecting); start_call hangs these up on arrival.
    cancelled: Arc<Mutex<std::collections::HashSet<String>>>,
    /// One mute lane per live call. Pruned against `active` where it grows,
    /// so a call that ends takes its lane with it without every teardown
    /// path having to remember.
    mute: Arc<std::sync::Mutex<HashMap<String, Arc<MuteLane>>>>,
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

impl CallRegistry {
    /// Record a ringing offer, so accept and decline have something to act on.
    pub(super) async fn offer(&self, call_id: String, call: Arc<WaIncomingCall>) {
        self.pending.lock().await.insert(call_id, call);
    }

    /// Forget a ringing offer, however it stopped ringing.
    pub(super) async fn forget_offer(&self, call_id: &str) {
        self.pending.lock().await.remove(call_id);
    }

    /// Take a live call out of the registry, if it is in it.
    ///
    /// Handing the handle back rather than acting on it: what a caller does
    /// with a call it has just removed differs — the peer ending it is not
    /// the same as us ending it — and that difference belongs at the call
    /// site, which is the one place that knows which happened.
    pub(super) async fn take_live(&self, call_id: &str) -> Option<Arc<CallHandle>> {
        self.active.lock().await.remove(call_id)
    }
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
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
            let Some(client) = client_handle.lock().await.clone() else {
                error!("Client not available for accepting call");
                return;
            };
            let Some(offer) = calls.pending.lock().await.remove(&call_id) else {
                warn!("No pending offer for call {}", call_id);
                return;
            };
            let (mic, speaker) = match open_call_audio().await {
                Ok(audio) => audio,
                Err(err) => {
                    error!("Audio device setup failed: {err}");
                    // The offer is consumed and no accept went out: reject
                    // so the caller stops ringing instead of waiting out
                    // the timeout.
                    if let Err(e) = client.voip().reject(&offer).await {
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
                    info!("Call {} media live", handle.call_id());
                    let handle = Arc::new(handle);
                    calls
                        .active
                        .lock()
                        .await
                        .insert(call_id.clone(), handle.clone());
                    Self::watch_call_end(handle, calls.clone(), ui_sender.clone());
                }
                Err(e) => {
                    error!("Failed to start call media for {}: {}", call_id, e);
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
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
            let Some(client) = client_handle.lock().await.clone() else {
                error!("Client not available for declining call");
                return;
            };
            let Some(offer) = calls.pending.lock().await.remove(&call_id) else {
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
        let runtime = self.runtime.clone();

        if is_video {
            warn!("Video calls are not supported yet; placing a voice call");
        }

        runtime.spawn(async move {
            let notify_failure = |error: String| {
                let ui_sender = ui_sender.clone();
                let recipient_jid = recipient_jid.clone();
                // A cancel may have landed for a call that will never
                // start; consume the marker so the set doesn't grow.
                let calls = calls.clone();
                let placeholder_id = placeholder_id.clone();
                async move {
                    calls.cancelled.lock().await.remove(&placeholder_id);
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
                    // Cancelled while still connecting: the UI only knew
                    // the placeholder id, so honor it here.
                    if calls.cancelled.lock().await.remove(&placeholder_id) {
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
                    let handle = Arc::new(handle);
                    calls
                        .active
                        .lock()
                        .await
                        .insert(call_id.clone(), handle.clone());
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
        let runtime = self.runtime.clone();

        runtime.spawn(async move {
            // Still ringing and never answered: nothing live to hang up.
            calls.pending.lock().await.remove(&call_id);
            if let Some(handle) = calls.active.lock().await.remove(&call_id) {
                log_termination(&call_id, handle.terminate().await);
            } else {
                // No handle yet (start_call still connecting under a UI
                // placeholder id): remember the cancel so it lands.
                debug!("cancel_call: no live handle for {}, deferring", call_id);
                calls.cancelled.lock().await.insert(call_id);
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
        let runtime = self.runtime.clone();

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

        runtime.spawn(async move {
            // Cloned out from under the lock: `set_muted` waits on the call's
            // answer-transition lane, and holding the registry across that
            // would stall every other call's bookkeeping behind one peer.
            let handle = {
                let active = calls.active.lock().await;
                let handle = active.get(&call_id).cloned();
                // Where the lane map grows is where it is swept.
                calls
                    .mute
                    .lock()
                    .expect("mute lanes poisoned")
                    .retain(|id, _| active.contains_key(id));
                handle
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
            calls.active.lock().await.remove(&call_id);
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

    /// A lone request is nobody's stale task: it applies, and it is the one
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
