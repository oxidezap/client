//! Calls, which are the one thing the session does that a page cannot.
//!
//! Split out of `whatsapp.rs` because it is the only part of the session that
//! reaches for `whatsapp_rust::voip` — a stack whose codec is C and does not
//! build for `wasm32-unknown-unknown` — and, since video calls, for a camera
//! and an encoder that are the same kind of thing. Keeping it in one file is
//! what lets the browser build leave it out without a `cfg` appearing
//! anywhere in the session's own logic.
//!
//! The methods are still `WhatsAppClient`'s; only the file changed. That is
//! the same split the GUI already uses for `app/`.

use super::super::*;
// Named here rather than through the star above, because the camera is this
// file's business alone: the session's own module has no use for one.
use crate::video::LocalVideo;
use oxidezap_audio::open_call_audio;
use whatsapp_rust::voip::{CallEvent, CallHandle, CallTermination, VideoState, VideoUpgradeToken};

/// Whether an offer asked for video.
///
/// Read off the offer rather than trusted from the front end: what the card
/// was drawn as and what the caller actually offered are two different
/// claims, and only one of them decides whether a video answer is even legal
/// (the library refuses `.video()` on an audio offer).
fn offered_video(offer: &WaIncomingCall) -> bool {
    matches!(&offer.action, CallAction::Offer { is_video, .. } if *is_video)
}

/// Clears an accept from the in-flight set however its task ends.
///
/// A guard rather than a line at each exit: the accept path returns from a
/// dozen places — a device that would not open, a refusal, a hangup — and the
/// set is the only thing that tells an ending call there is somebody to leave
/// a note for. One missed exit leaves a note nobody will ever read, for a
/// call that will never come back.
struct AcceptGuard {
    calls: CallRegistry,
    call_id: String,
}

impl Drop for AcceptGuard {
    fn drop(&mut self) {
        // Cleared here rather than from a spawned task. `abandon_accept` is
        // synchronous, and deferring it left the accept marked in flight
        // after it had ended, so a decline landing there was answered
        // `Declined::Accepting` by an acceptance already gone and the caller
        // went on ringing.
        self.calls.abandon_accept(&self.call_id);
    }
}

/// What became of a camera handed to [`CallRegistry::hold_camera`].
///
/// Three answers rather than a `bool`, because only one of them leaves the
/// peer holding a pane open: a call that ended is one nobody is announcing
/// anything to, and a device that died on a live call is a direction the far
/// side still believes in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Camera {
    Held,
    CallEnded,
    Died,
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
///
/// Video widened what the lock covers rather than adding locks beside it. A
/// camera is registered against a call that may have ended while the device
/// was opening, and the answer to "is this call still live" has to be true
/// for as long as it takes to act on — which is what one lock gives and two
/// only approximate.
#[derive(Clone, Default)]
pub struct CallRegistry {
    /// A `std` lock, and every method that takes it is synchronous.
    ///
    /// Not a preference: an outgoing call has to be marked in flight on the
    /// caller's thread, *before* its task exists, for the same reason a mute
    /// request is stamped there — spawning is not sequencing, and a cancel
    /// spawned a moment later can run first. An async lock would force that
    /// mark into the task, which is exactly where it is too late: the cancel
    /// would find nothing in flight, decline to leave a note, and the call it
    /// meant to stop would go on to ring at the far end with every window
    /// already showing it gone.
    ///
    /// Safe because no critical section here spans an await. They are all a
    /// handful of map operations; the two that then have to *wait* on
    /// something — closing a device, hanging up — take what they need out
    /// under the lock and release it before they do.
    calls: Arc<std::sync::Mutex<Calls>>,
    /// Apart, deliberately. A mute lane is a `std::sync::Mutex` because a
    /// request is stamped on the caller's thread *before* its task exists —
    /// see [`MuteLane`] — and it takes no part in the ringing/live/cancelled
    /// invariants above.
    mute: Arc<std::sync::Mutex<HashMap<String, Arc<MuteLane>>>>,
    /// The same, for cameras. See [`VideoLane`].
    video: Arc<std::sync::Mutex<HashMap<String, Arc<VideoLane>>>>,
}

/// Which side ended a call that had no handle yet.
///
/// A handle produced after the fact has to be torn down either way, and the
/// difference is what goes on the wire: the peer already knows when it was
/// theirs, and nobody knows when it was ours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::whatsapp) enum Ending {
    /// Ours: hung up here, so every device the offer rang has to be told.
    Local,
    /// Theirs, or another of our devices: saying it again tells nobody
    /// anything.
    Remote,
}

/// What a decline found to act on.
///
/// Four answers rather than "the offer or nothing", because a decline can
/// arrive at any moment of a call's short life and three of those moments
/// are not a ringing offer. A second window's Decline can land after this
/// one's Accept has taken the offer, or after it has a live handle; reading
/// only `pending` there found nothing and left the acceptance to register a
/// call every window had already cleared.
pub(in crate::whatsapp) enum Declined {
    /// Still ringing: reject it, and the caller stops.
    Ringing(Arc<WaIncomingCall>),
    /// An acceptance is in flight and has been told to stop. It sends the
    /// rejection itself, because it is the one holding the offer.
    Accepting,
    /// Already live: this is a hangup wearing a decline's name.
    Live(Arc<CallHandle>),
    /// No such call.
    Nothing,
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
    /// Calls ended before any handle existed — the UI's placeholder id while
    /// `start_call` is still connecting, or a peer's `<terminate>` arriving
    /// while `accept_call` is opening the microphone. Whichever call is in
    /// flight hangs these up on arrival.
    ///
    /// Keyed to *who* ended it, because the two want different endings and a
    /// handle that arrives late still has to send one of them. See [`Ending`].
    cancelled: HashMap<String, Ending>,
    /// Acceptances and placements in flight: no handle exists yet. Opening
    /// the audio devices, opening a camera and connecting the relay all take
    /// time, and an ending arriving in that window has nowhere else to be
    /// written down.
    in_flight: HashSet<String>,
    /// The camera feeding each call whose local direction is on. Absent is
    /// the whole of "our video is off": there is no second flag to disagree
    /// with, and removing the entry is what closes the device.
    cameras: HashMap<String, LocalVideo>,
    /// A peer's outstanding request to turn a call into a video one.
    ///
    /// Kept here rather than handed to the front end because the token is
    /// what binds an answer to *that* request, and only this process can use
    /// it. Turning the camera on while one is parked answers it; turning it
    /// on with none parked asks a question of our own.
    upgrades: HashMap<String, VideoUpgradeToken>,
    /// Calls with an upgrade of *ours* still waiting on an answer.
    ///
    /// Presence, not identity, and that is the whole of it: the library does
    /// not match a refusal to the request it refuses. Its handler tears the
    /// local plane down whenever *some* request of ours is outstanding —
    /// whichever camera is attached by then — and ignores the stanza entirely
    /// when none is. So the question this has to answer is "did the library
    /// just release our endpoints", and this flag is exactly that. Keying on
    /// the camera the request went out with would leave a camera registered
    /// and drawn after its media plane was already gone, which is the same
    /// lie in the other direction.
    upgrading: HashSet<String>,
}

impl CallRegistry {
    /// Record a ringing offer, so accept and decline have something to act on.
    pub(in crate::whatsapp) fn offer(&self, call_id: String, call: Arc<WaIncomingCall>) {
        self.calls
            .lock()
            .expect("call registry poisoned")
            .pending
            .insert(call_id, call);
    }

    /// Take a ringing offer and mark its acceptance as in flight, together.
    ///
    /// One operation because they are one step: between leaving `pending` and
    /// entering `in_flight` the call would be in nothing at all, which is the
    /// state that has no answer for a peer hanging up.
    pub(in crate::whatsapp) fn begin_accept(&self, call_id: &str) -> Option<Arc<WaIncomingCall>> {
        let mut calls = self.calls.lock().expect("call registry poisoned");
        let offer = calls.pending.remove(call_id)?;
        calls.in_flight.insert(call_id.to_string());
        Some(offer)
    }

    /// File an accepted call as live — unless it ended meanwhile.
    ///
    /// `Some(ending)` means the window has already been told the call is
    /// over, so the handle is not filed and the caller ends it — in the way
    /// that ending calls for. Both answers leave `in_flight` empty for this
    /// id.
    pub(in crate::whatsapp) fn finish_accept(
        &self,
        call_id: &str,
        handle: &Arc<CallHandle>,
    ) -> Option<Ending> {
        let mut calls = self.calls.lock().expect("call registry poisoned");
        calls.in_flight.remove(call_id);
        if let Some(ending) = calls.cancelled.remove(call_id) {
            return Some(ending);
        }
        calls.active.insert(call_id.to_string(), Arc::clone(handle));
        None
    }

    /// An acceptance that produced no handle. Says whether somebody had
    /// already ended it, so a caller knows whether anyone is still ringing.
    pub(in crate::whatsapp) fn abandon_accept(&self, call_id: &str) -> bool {
        let mut calls = self.calls.lock().expect("call registry poisoned");
        calls.in_flight.remove(call_id);
        calls.cancelled.remove(call_id).is_some()
    }

    /// Why this call ended, if somebody ended it while the acceptance was
    /// still opening its devices.
    ///
    /// Consumed, because the caller acts on it and returns: a note left
    /// behind would be one the guard clears without anybody having answered
    /// it. What the answer *is* differs by cause and neither is nothing —
    /// ours means the offer was taken out of `pending` and no accept ever
    /// went out, so every device it rang is still ringing until it is
    /// rejected; theirs means they already know.
    pub(in crate::whatsapp) fn ended_meanwhile(&self, call_id: &str) -> Option<Ending> {
        self.calls
            .lock()
            .expect("call registry poisoned")
            .cancelled
            .remove(call_id)
    }

    /// Mark an outgoing call as being placed, under the id the window drew.
    pub(in crate::whatsapp) fn begin_start(&self, placeholder: &str) {
        self.calls
            .lock()
            .expect("call registry poisoned")
            .in_flight
            .insert(placeholder.to_string());
    }

    /// File a placed call as live under its real id — unless it was cancelled
    /// while connecting, in which case the caller terminates it.
    ///
    /// The placeholder is what a cancel names, because it is the only id the
    /// window had; the rename and the cancellation are answered together so a
    /// cancel arriving between them cannot be lost.
    pub(in crate::whatsapp) fn finish_start(
        &self,
        placeholder: &str,
        call_id: &str,
        handle: &Arc<CallHandle>,
    ) -> bool {
        let mut calls = self.calls.lock().expect("call registry poisoned");
        calls.in_flight.remove(placeholder);
        // Either name: the window cancels under the placeholder, and anything
        // that learned the real id first cancels under that.
        let cancelled = calls.cancelled.remove(placeholder).is_some()
            | calls.cancelled.remove(call_id).is_some();
        if cancelled {
            return false;
        }
        calls.active.insert(call_id.to_string(), Arc::clone(handle));
        true
    }

    /// An outgoing call that never produced a handle.
    pub(in crate::whatsapp) fn abandon_start(&self, placeholder: &str) {
        let mut calls = self.calls.lock().expect("call registry poisoned");
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
    ///
    /// One method for every event that means "this call is over and it was
    /// not us": a peer's `<terminate>` or `<reject>`, an offer that stopped
    /// ringing, another of our devices taking it. They differ in what the
    /// window is told and in nothing else, and each of them used to say only
    /// half of this — the offer forgotten, the acceptance in flight not — so
    /// an accept still opening its microphone would file a live handle behind
    /// a card every window had already cleared, leaving audible call with
    /// nothing to end it.
    pub(in crate::whatsapp) fn ended_remotely(&self, call_id: &str) {
        let mut calls = self.calls.lock().expect("call registry poisoned");
        calls.pending.remove(call_id);
        if let Some(handle) = calls.active.remove(call_id) {
            // The session's executor rather than tokio's: a page has no runtime
            // to reach for. Dropped rather than kept, which detaches on both --
            // a tokio `JoinHandle` and the page's oneshot receiver each leave
            // their task running -- and this is a hangup nobody waits on.
            drop(crate::exec::spawn(
                async move { handle.hangup_local().await },
            ));
            return;
        }
        if calls.in_flight.contains(call_id) {
            calls.cancelled.insert(call_id.to_string(), Ending::Remote);
        }
    }

    /// Ask for a live call without taking it.
    pub(in crate::whatsapp) fn live(&self, call_id: &str) -> Option<Arc<CallHandle>> {
        self.calls
            .lock()
            .expect("call registry poisoned")
            .active
            .get(call_id)
            .cloned()
    }

    /// A live call's handle, and a sweep of the lanes while the answer to
    /// "what is live" is in hand.
    ///
    /// Where the lane maps grow is where they are swept: a lane is made by a
    /// request naming a call, and a call that is no longer live is one no
    /// further request can reach.
    fn live_and_sweep(&self, call_id: &str) -> Option<Arc<CallHandle>> {
        let calls = self.calls.lock().expect("call registry poisoned");
        self.mute
            .lock()
            .expect("mute lanes poisoned")
            .retain(|id, _| calls.active.contains_key(id));
        self.video
            .lock()
            .expect("video lanes poisoned")
            .retain(|id, _| calls.active.contains_key(id));
        calls.active.get(call_id).cloned()
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
    pub(in crate::whatsapp) fn cancel(&self, call_id: &str) -> Cancelled {
        let mut calls = self.calls.lock().expect("call registry poisoned");
        calls.pending.remove(call_id);
        if let Some(handle) = calls.active.remove(call_id) {
            return Cancelled::Live(handle);
        }
        if calls.in_flight.contains(call_id) {
            calls.cancelled.insert(call_id.to_string(), Ending::Local);
            return Cancelled::Deferred;
        }
        Cancelled::Nothing
    }

    /// What a pending ending says, for tests.
    ///
    /// The real reader is [`Self::finish_accept`], which consumes it and
    /// needs a `CallHandle` to be given one — a type nothing outside the
    /// media stack can build.
    #[cfg(test)]
    pub(in crate::whatsapp) fn ending_for(&self, call_id: &str) -> Option<Ending> {
        self.calls
            .lock()
            .expect("call registry poisoned")
            .cancelled
            .get(call_id)
            .copied()
    }

    /// Decline a call, whatever stage it has reached.
    ///
    /// One operation, because the four answers are decided by the same state
    /// and a decline that read them one at a time would miss the call moving
    /// between two reads — which is exactly how a stale Decline from a second
    /// window used to find nothing and let an acceptance go on to register a
    /// live handle behind a cleared card.
    pub(in crate::whatsapp) fn decline(&self, call_id: &str) -> Declined {
        let mut calls = self.calls.lock().expect("call registry poisoned");
        if let Some(offer) = calls.pending.remove(call_id) {
            return Declined::Ringing(offer);
        }
        if let Some(handle) = calls.active.remove(call_id) {
            return Declined::Live(handle);
        }
        if calls.in_flight.contains(call_id) {
            calls.cancelled.insert(call_id.to_string(), Ending::Local);
            return Declined::Accepting;
        }
        Declined::Nothing
    }

    /// Mark an acceptance in flight without an offer, for tests.
    ///
    /// [`Self::begin_accept`] is the real entry and takes a `WaIncomingCall`,
    /// which is `#[non_exhaustive]` upstream and so cannot be built here at
    /// all. What these tests are about is where a call *is* — the transition
    /// between in-flight and live-or-cancelled — rather than what its offer
    /// said, and this reaches that state directly.
    #[cfg(test)]
    pub(in crate::whatsapp) fn mark_accepting(&self, call_id: &str) {
        self.calls
            .lock()
            .expect("call registry poisoned")
            .in_flight
            .insert(call_id.to_string());
    }
}

/// The camera half, under the same lock as everything else.
impl CallRegistry {
    /// Put a camera in the registry, or take it straight back down when there
    /// is nothing left to hold it for.
    ///
    /// Both questions — is the device still there, is the call still there —
    /// are asked *after* the insertion and under the lock the teardowns take,
    /// and that is the whole of it. Asked before, each leaves a window in
    /// which the cleanup runs against a registry this camera is not in yet:
    /// it finds nothing to remove and finishes, and the entry made a moment
    /// later is the one nothing ever comes back for — the device staying
    /// open, with its light on, until the daemon exits.
    ///
    /// One lock is what makes the answers true for long enough to act on. The
    /// pump clears `alive` before it reports a loss, so a dead camera is
    /// either already gone from the map or visible right here; and a call
    /// that ended has left `active` under this same lock, so "still live" and
    /// "camera filed" cannot disagree between two acquisitions.
    async fn hold_camera(&self, call_id: &str, local: LocalVideo) -> Camera {
        let taken = {
            let mut calls = self.calls.lock().expect("call registry poisoned");
            calls.cameras.insert(call_id.to_string(), local);
            let ended = !calls.active.contains_key(call_id);
            let dead = !calls.cameras.get(call_id).is_some_and(LocalVideo::alive);
            if ended || dead {
                calls.cameras.remove(call_id).map(|held| (held, ended))
            } else {
                None
            }
        };
        let Some((taken, ended)) = taken else {
            return Camera::Held;
        };
        // Outside the lock: closing a device waits for its capture thread,
        // and every other call's bookkeeping would queue behind it.
        taken.stop().await;
        if ended {
            info!("Call {call_id} ended while its camera was being wired up");
            Camera::CallEnded
        } else {
            warn!("The camera on call {call_id} died while it was being wired up");
            Camera::Died
        }
    }

    /// The call this camera belongs to has just become live: hand the device
    /// the two things a newly drawable call needs, and say whether there was
    /// one.
    ///
    /// A camera opened while the call was still ringing has been encoding
    /// into a stream nobody could draw — the state had no live call to put a
    /// direction in — so the first unit any decoder now starting sees would
    /// reference frames it never got.
    pub(in crate::whatsapp) fn camera_became_drawable(&self, call_id: &str) -> bool {
        let calls = self.calls.lock().expect("call registry poisoned");
        let Some(local) = calls.cameras.get(call_id) else {
            return false;
        };
        local.drawable();
        local.request_keyframe();
        true
    }

    /// Whether this side's camera is on for this call.
    fn camera_on(&self, call_id: &str) -> bool {
        self.calls
            .lock()
            .expect("call registry poisoned")
            .cameras
            .contains_key(call_id)
    }

    /// Take this call's camera out, for a caller that is going to close it.
    ///
    /// Taken rather than borrowed under the lock: closing waits on a capture
    /// thread, and holding the registry across that wait stalls every other
    /// call.
    fn take_camera(&self, call_id: &str) -> Option<LocalVideo> {
        self.calls
            .lock()
            .expect("call registry poisoned")
            .cameras
            .remove(call_id)
    }

    /// The same, but only if it is still the camera the caller means.
    ///
    /// `only` names the camera a teardown was scheduled for; the work is
    /// spawned, so the camera in the registry may be a later one. `None` is
    /// "whatever is there now", which is right for something learned from the
    /// peer in the moment.
    fn take_camera_if(
        &self,
        call_id: &str,
        only: Option<crate::video::CameraId>,
    ) -> Option<LocalVideo> {
        let mut calls = self.calls.lock().expect("call registry poisoned");
        match calls.cameras.get(call_id) {
            Some(held) if only.is_none_or(|wanted| held.camera_id() == wanted) => {
                calls.cameras.remove(call_id)
            }
            _ => None,
        }
    }

    /// Ask this call's camera for a keyframe, if it has one.
    fn ask_for_keyframe(&self, call_id: &str) {
        if let Some(local) = self
            .calls
            .lock()
            .expect("call registry poisoned")
            .cameras
            .get(call_id)
        {
            local.request_keyframe();
        }
    }

    /// Ask every live camera, for a subscriber who has never seen one.
    fn ask_all_for_keyframes(&self) {
        for local in self
            .calls
            .lock()
            .expect("call registry poisoned")
            .cameras
            .values()
        {
            local.request_keyframe();
        }
    }

    /// Park the peer's request to go to video, so turning the camera on can
    /// answer it.
    fn park_upgrade(&self, call_id: &str, token: VideoUpgradeToken) {
        self.calls
            .lock()
            .expect("call registry poisoned")
            .upgrades
            .insert(call_id.to_string(), token);
    }

    /// Take the peer's parked request, which is what answering it costs.
    fn take_upgrade(&self, call_id: &str) -> Option<VideoUpgradeToken> {
        self.calls
            .lock()
            .expect("call registry poisoned")
            .upgrades
            .remove(call_id)
    }

    /// Record that an upgrade of *ours* is waiting on the peer's answer.
    fn begin_upgrade(&self, call_id: &str) {
        self.calls
            .lock()
            .expect("call registry poisoned")
            .upgrading
            .insert(call_id.to_string());
    }

    /// Withdraw it, and say whether there was one — which is the whole
    /// question a refusal has to answer.
    fn end_upgrade(&self, call_id: &str) -> bool {
        self.calls
            .lock()
            .expect("call registry poisoned")
            .upgrading
            .remove(call_id)
    }

    /// A call has ended: clear everything keyed to it and hand back the
    /// camera for the caller to close.
    ///
    /// One acquisition rather than five, which is not only tidier: a request
    /// arriving mid-teardown would otherwise see a call with no handle but a
    /// camera still filed, or an upgrade still outstanding against a call
    /// that has none.
    fn ended(&self, call_id: &str) -> Option<LocalVideo> {
        let mut calls = self.calls.lock().expect("call registry poisoned");
        calls.active.remove(call_id);
        calls.upgrades.remove(call_id);
        calls.upgrading.remove(call_id);
        // Every call that ever had a handle drains through here, whatever
        // ended it, so this is where a lane is paid for. The sweep in
        // `live_and_sweep` is not made redundant by it: a window that fell
        // behind can stamp a request against a call this watcher has already
        // run for, and that lane has no second ending to be removed on.
        self.mute
            .lock()
            .expect("mute lanes poisoned")
            .remove(call_id);
        self.video
            .lock()
            .expect("video lanes poisoned")
            .remove(call_id);
        // The camera outlives nothing: a call that ended with video on would
        // otherwise keep the device open, with its light on, for as long as
        // the process lived.
        calls.cameras.remove(call_id)
    }

    /// The lane serializing one call's camera transitions, made if nothing
    /// has wanted it yet.
    fn video_lane(&self, call_id: &str) -> Arc<VideoLane> {
        self.video
            .lock()
            .expect("video lanes poisoned")
            .entry(call_id.to_string())
            .or_default()
            .clone()
    }

    /// The lane serializing one call's mute requests, made if nothing has
    /// wanted it yet.
    fn mute_lane(&self, call_id: &str) -> Arc<MuteLane> {
        self.mute
            .lock()
            .expect("mute lanes poisoned")
            .entry(call_id.to_string())
            .or_default()
            .clone()
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

/// What keeps a call's camera requests in the order the daemon took them.
///
/// The same problem the mute lane exists for, and worse: opening a camera is
/// device work — tens of milliseconds, and the first time a permission
/// prompt — so two requests spawned in order routinely *start* in the other.
/// Without a stamp taken before the spawn, an "off" that overtook an "on"
/// would leave the device open under a state saying it was closed, and a
/// second window's request could open a camera the first had just released.
#[derive(Default)]
struct VideoLane {
    /// The newest request, stamped on the caller's thread before its task
    /// exists. That is the only place the order still exists.
    intent: std::sync::Mutex<VideoIntent>,
    /// One camera transition in flight per call: the device itself is the
    /// resource being serialized, and two opens of it race in the driver.
    lane: Mutex<()>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct VideoIntent {
    seq: u64,
    on: bool,
}

impl WhatsAppClient {
    /// Accept an incoming call: signaling, callKey decrypt, relay connect and
    /// the audio engine are all inside `client.voip().accept(..)`; this side
    /// only supplies the cpal mic/speaker bridge and, for a video answer, the
    /// camera.
    pub fn accept_call(&self, call_id: &str, with_video: bool) {
        let client_handle = self.client_handle.clone();
        let calls = self.calls.clone();
        let ui_sender = self.ui_sender.clone();
        let publish = self.video_publisher();
        let lost = self.camera_lost();
        let call_id = call_id.to_string();

        // On the caller's thread, before the spawn, and it is the same reason
        // `start_call` marks itself there: spawning is not sequencing. Accept
        // followed straight by Hang Up produces two tasks, and the cancel's
        // can run first — taking the offer out of `pending`, finding neither
        // a live call nor one in flight, and answering `Nothing`. The accept
        // would then find no offer and return having sent nothing at all: no
        // accept, no reject, no terminate, and a caller left ringing until
        // their own timeout with every window already showing the call gone.
        //
        // Taking the offer and marking the acceptance is one step under one
        // lock, because between them the call would be in nothing at all —
        // which is the state that has no answer for a peer hanging up.
        let Some(offer) = self.calls.begin_accept(&call_id) else {
            warn!("No pending offer for call {}", call_id);
            return;
        };

        self.exec.spawn(async move {
            // First, so that every path out of here clears the in-flight mark
            // — including the one below, which is the only one that can fail
            // before anything has been said to anybody.
            let _accepting = AcceptGuard {
                calls: calls.clone(),
                call_id: call_id.clone(),
            };
            let Some(client) = client_handle.lock().await.clone() else {
                error!("Client not available for accepting call");
                return;
            };
            let (mic, speaker, audio_facts) = match open_call_audio().await {
                Ok(audio) => audio,
                Err(err) => {
                    error!("Audio device setup failed: {err}");
                    // The offer is consumed and no accept went out: reject so
                    // the caller stops ringing instead of waiting out the
                    // timeout.
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
            // The camera is opened before the accept goes out, so an offer
            // answered with video is one this side can actually send. A
            // camera that will not open is not a reason to refuse the call:
            // the answer is audio, which is exactly what a phone does when
            // its camera is busy.
            let video = if with_video {
                match video::open(video::slot(&call_id), publish, lost).await {
                    Ok(video) => Some(video),
                    Err(err) => {
                        warn!("Answering call {call_id} without video: {err}");
                        None
                    }
                }
            } else {
                None
            };
            let (local, endpoints) = match video {
                Some((local, endpoints)) => (Some(local), Some(endpoints)),
                None => (None, None),
            };

            // Ended while the camera was opening — hung up here, hung up by
            // the caller, or taken on another device. Seconds, the first time
            // a permission prompt, and the `<accept>` below would answer a
            // call nobody is on any more. The registration consumes the same
            // note under the lock; this is only about not sending the stanza.
            if let Some(ending) = calls.ended_meanwhile(&call_id) {
                info!("Call {} ended before its media came up", call_id);
                if let Some(local) = local {
                    local.stop().await;
                }
                // Ours, and no accept ever went out: the offer left `pending`
                // when this started, so nothing else is going to answer it and
                // every device it rang is still ringing. Rejecting is what
                // stops them, and it is the same thing the audio-failure path
                // above does for the same reason.
                if ending == Ending::Local
                    && let Err(e) = client.voip().reject(&offer).await
                {
                    error!(
                        "Failed to reject call {} after a local hangup: {}",
                        call_id, e
                    );
                }
                Self::notify_call_ended(&ui_sender, &call_id).await;
                return;
            }

            let answered_with_video = endpoints.is_some();
            let voip = client.voip();
            let accept = voip.accept(&offer).audio(mic, speaker);
            let accept = match endpoints {
                Some(endpoints) => accept.video(endpoints.source, endpoints.sink),
                None => accept,
            };
            // Before the `start()`, not after it. `start()` awaits, and what
            // it spawns is the driver — so on a page, where every task shares
            // one loop, a driver that takes the endpoints and returns *while
            // this is still pending* drops them before a mark placed after
            // the await could run. That is precisely the failure being
            // hunted, so a mark there would report `NeverHandedOver` for the
            // one call it exists to describe. Here is where ownership really
            // moves: the builder holds the endpoints, nothing above may
            // return any more, and no `await` separates this from the call
            // that hands it over.
            audio_facts.hand_to_engine();
            match accept.start().await {
                Ok(handle) => {
                    let handle = Arc::new(handle);
                    // Hung up while the camera was opening. Answering a video
                    // call waits on a device — and, the first time, on a
                    // permission prompt — with the card possibly already gone
                    // from every window, so a call registered here would be
                    // one nobody could see or end. The check and the
                    // registration are one operation under one lock; as two
                    // steps there is a gap in which a hangup does neither.
                    if let Some(ending) = calls.finish_accept(&call_id, &handle) {
                        info!("Call {} was hung up while its media came up", call_id);
                        if let Some(local) = local {
                            local.stop().await;
                        }
                        match ending {
                            // Ours: the offer rang somewhere, and every device
                            // it rang is still ringing until it is told.
                            Ending::Local => {
                                log_termination(&call_id, handle.terminate().await);
                            }
                            // Theirs, or another of our devices. Answering a
                            // `<terminate>` with one of our own says nothing
                            // they do not already know; only the local media
                            // task and this handle are left to drop.
                            Ending::Remote => handle.hangup_local().await,
                        }
                        return;
                    }
                    info!("Call {} media live", handle.call_id());
                    // What this call turned out to be. The state was built
                    // from the offer the moment the answer was given, and a
                    // camera that would not open answers a video offer as a
                    // voice call rather than refusing it — which only this
                    // side knows.
                    if let Some(tx) = ui_sender.lock().await.as_ref() {
                        let _ = tx.send(UiEvent::CallAnswered {
                            call_id: call_id.clone(),
                            is_video: answered_with_video,
                        });
                    }
                    if let Some(local) = local {
                        // There is a call to draw into now — and the encoder
                        // has been running since before the accept went out,
                        // with its opening IDR published nowhere. The decoder
                        // that starts on the first frame to arrive has
                        // nothing to start from until the next one, seconds
                        // away, so it is asked for here.
                        local.drawable();
                        local.request_keyframe();
                        match calls.hold_camera(&call_id, local).await {
                            Camera::Held => {
                                Self::announce_video(
                                    &ui_sender,
                                    &call_id,
                                    VideoStream::Local,
                                    true,
                                )
                                .await;
                            }
                            // The accept said this call had video, so the peer
                            // is holding a pane open for a device that is
                            // gone. Nothing was announced here, so the state
                            // already says what is true on this side.
                            Camera::Died => Self::stop_peer_video(&handle, &call_id).await,
                            Camera::CallEnded => {}
                        }
                    }
                    // A call offered as video has the caller's camera on by
                    // definition; the peer's own `<video>` corrects this if
                    // they turn it off. Only when this side answered *with*
                    // video, though: an offer answered without endpoints has
                    // no plane for their frames to arrive on, and a window
                    // told otherwise waits out the call in front of a pane
                    // nothing can ever fill.
                    if answered_with_video && offered_video(&offer) {
                        Self::announce_video(&ui_sender, &call_id, VideoStream::Remote, true).await;
                    }
                    Self::watch_call(handle, calls.clone(), ui_sender.clone());
                }
                Err(e) => {
                    error!("Failed to start call media for {}: {}", call_id, e);
                    if let Some(local) = local {
                        local.stop().await;
                    }
                    Self::notify_call_ended(&ui_sender, &call_id).await;
                }
            }
        });
    }

    /// Ask every live camera for a keyframe, because somebody is about to
    /// draw who has never seen one.
    ///
    /// A window attaching mid-call subscribes to the stream where it happens
    /// to be, which is a P-frame referencing units published before it was
    /// listening. Its decoder can do nothing with those, so the self-view
    /// stays empty until the encoder's own periodic IDR — seconds, on a
    /// picture the person just opened a window to see. The same rule every
    /// other moment a decoder is born follows.
    ///
    /// Every camera rather than one call's: this is asked when a subscriber
    /// arrives, and a subscriber draws whatever the daemon is holding.
    pub fn request_video_keyframe(&self) {
        let calls = self.calls.clone();
        self.exec.spawn(async move {
            calls.ask_all_for_keyframes();
        });
    }

    /// Turn this side's camera on or off during a live call.
    ///
    /// The two directions of a call's video are independent and each side
    /// owns its own, so this is only ever about ours. Turning it on answers
    /// the peer's request when there is one parked — the token is what binds
    /// the answer to that request — and asks one of our own when there is
    /// not.
    ///
    /// Like mute, what is published is what the device ended up doing rather
    /// than what was asked for: a camera that will not open, or an
    /// announcement the peer never got, would otherwise leave a front end
    /// drawing a picture nobody is being sent.
    pub fn set_call_video(&self, call_id: &str, on: bool) {
        let calls = self.calls.clone();
        let ui_sender = self.ui_sender.clone();
        let publish = self.video_publisher();
        let lost = self.camera_lost();
        let call_id = call_id.to_string();

        // Before the spawn, because after it the order is gone. See
        // [`VideoLane`].
        let (lane, seq) = {
            let lane = calls.video_lane(&call_id);
            let mut intent = lane.intent.lock().expect("video intent poisoned");
            intent.seq += 1;
            intent.on = on;
            let seq = intent.seq;
            drop(intent);
            (lane, seq)
        };

        self.exec.spawn(async move {
            let Some(handle) = calls.live_and_sweep(&call_id) else {
                debug!("set_call_video: no live handle for {}", call_id);
                // Answered rather than dropped. A window draws the camera as
                // coming on the moment it is asked, and it clears that on the
                // settle — so a request that arrives in the seconds between
                // the state saying a call is live and this side registering
                // its handle would otherwise leave the control lit for the
                // rest of the call, and its next click asking to turn off a
                // camera that was never opened. What the registry holds is
                // nothing, which is exactly what is said.
                Self::settle_video(&calls, &ui_sender, &call_id, seq, &lane).await;
                return;
            };

            let _serialized = lane.lane.lock().await;
            // A newer request either has already run or is queued behind us
            // on this lane; either way it, and not this one, is what the
            // device should end up saying. Staying silent is the point: a
            // superseded task that announced its own value would restore it
            // over the newer one.
            if lane.intent.lock().expect("video intent poisoned").seq != seq {
                return;
            }

            if !on {
                // Taken out of the registry before it is waited on: closing a
                // device means waiting for its capture thread, and every
                // other call's bookkeeping would queue behind it.
                if let Some(local) = calls.take_camera(&call_id) {
                    // The device is released first, matching `stop_video`
                    // itself: the user asked for the camera to go off, and a
                    // failed stanza must not leave it running.
                    local.stop().await;
                }
                Self::stop_peer_video(&handle, &call_id).await;
                // `stop_video` clears the library's pending request, so a
                // refusal after it is one the library ignores — and so is
                // this.
                calls.end_upgrade(&call_id);
                Self::settle_video(&calls, &ui_sender, &call_id, seq, &lane).await;
                return;
            }

            if calls.camera_on(&call_id) {
                // Already on. Said again rather than returning silently: this
                // is the newest request, and what it costs to restate is
                // nothing — the daemon publishes no frame for a state that
                // did not change.
                Self::settle_video(&calls, &ui_sender, &call_id, seq, &lane).await;
                return;
            }
            let (local, endpoints) = match video::open(video::slot(&call_id), publish, lost).await {
                Ok(video) => video,
                Err(err) => {
                    error!("Camera setup failed for call {call_id}: {err}");
                    // Said out loud rather than left silent: the front end
                    // drew the camera as coming on the moment it was asked.
                    Self::settle_video(&calls, &ui_sender, &call_id, seq, &lane).await;
                    return;
                }
            };
            // Asked again, because opening a device is where the time goes —
            // tens of milliseconds, the first time a permission prompt — and
            // the lane held whatever came after us off for all of it. Going
            // on from here is not a word that can be taken back: it spends
            // the peer's upgrade token and starts transmitting, which the
            // "off" queued behind us would then have to undo.
            if lane.intent.lock().expect("video intent poisoned").seq != seq {
                local.stop().await;
                return;
            }
            // Consuming the token *is* the answer, so the question goes with
            // it — every window drawing it has to stop.
            let answering = calls.take_upgrade(&call_id);
            if answering.is_some() {
                Self::announce_video_request(&ui_sender, &call_id, false).await;
            }
            // Whether the peer owes us an answer: an upgrade we asked for is
            // accepted or refused seconds later, and what that answer means
            // depends on a request of ours still being outstanding when it
            // lands. Answering one of *theirs* owes nothing.
            let ours_to_be_answered = answering.is_none();
            // Recorded before the request goes out, for the reason every
            // other intent here is stamped before its task exists: the reply
            // is not ours to schedule. `start_video` puts `<video_state>` on
            // the wire and the peer's refusal comes back on the event stream,
            // which is a different task — so a fast peer, or a signaling
            // error answered by return, lands its reject while this is still
            // awaiting, finds nothing outstanding and lets the camera stand
            // while the library has already released the plane under it.
            // Registering late cannot be made safe by ordering; registering
            // early can, because every path out of here that is not a camera
            // held withdraws it again.
            if ours_to_be_answered {
                calls.begin_upgrade(&call_id);
            }
            let started = match answering {
                Some(token) => {
                    handle
                        .accept_video(token, endpoints.source, endpoints.sink)
                        .await
                }
                None => handle.start_video(endpoints.source, endpoints.sink).await,
            };
            match started {
                Ok(()) => {
                    // And asked once more, because signaling is another await
                    // and the newest request is the only one that may speak.
                    // Unlike the check before it this one has something to
                    // undo: the direction is negotiated, so the peer is told
                    // it stopped as well as the device being closed — an
                    // "off" queued behind us would otherwise find a camera
                    // that was never registered, no picture of its own to
                    // stop, and a peer still holding a pane open.
                    if lane.intent.lock().expect("video intent poisoned").seq != seq {
                        local.stop().await;
                        Self::stop_peer_video(&handle, &call_id).await;
                        calls.end_upgrade(&call_id);
                        return;
                    }
                    // The call is already live, so the self-view has had
                    // somewhere to land since before the camera opened — and
                    // it had nowhere to land while the announcement was on
                    // the wire, which is where the opening IDR went. Whoever
                    // draws this starts a decoder on the first frame that
                    // arrives and can do nothing with it until a keyframe,
                    // which is otherwise the periodic one, seconds away.
                    local.drawable();
                    local.request_keyframe();
                    // The announcement landed and the device may not have
                    // survived it: the peer has this direction enabled and is
                    // waiting on a picture, and `settle_video` below says off
                    // only on this side. A call that ended in the meantime has
                    // nobody to tell.
                    //
                    // Either way there is no camera left for a refusal to take
                    // down, so the question we registered before asking it is
                    // withdrawn — a reject arriving later belongs to nothing
                    // of ours, which is exactly what its handler tests for.
                    match calls.hold_camera(&call_id, local).await {
                        Camera::Died => {
                            Self::stop_peer_video(&handle, &call_id).await;
                            calls.end_upgrade(&call_id);
                        }
                        Camera::CallEnded => {
                            calls.end_upgrade(&call_id);
                        }
                        Camera::Held => {}
                    }
                }
                Err(e) => {
                    error!("Failed to start video on call {}: {}", call_id, e);
                    local.stop().await;
                    calls.end_upgrade(&call_id);
                }
            }
            Self::settle_video(&calls, &ui_sender, &call_id, seq, &lane).await;
        });
    }

    /// Publish what the camera *is*, once the newest request has reached it.
    ///
    /// Read back from the registry rather than from what was asked for, which
    /// is the same rule mute follows and for the same reason: a camera that
    /// would not open, or an announcement the peer never got, leaves the
    /// device somewhere the request did not choose, and the front end has
    /// already drawn what it asked for.
    ///
    /// Silent when a newer request has arrived meanwhile: that one speaks
    /// after it has reached the device, which is what makes it the last word.
    async fn settle_video(
        calls: &CallRegistry,
        ui_sender: &UiEventSender,
        call_id: &str,
        seq: u64,
        lane: &VideoLane,
    ) {
        if lane.intent.lock().expect("video intent poisoned").seq != seq {
            return;
        }
        let settled = calls.camera_on(call_id);
        Self::announce_video(ui_sender, call_id, VideoStream::Local, settled).await;
    }

    /// Close this side's camera and say so, for the reasons that are not a
    /// request: the device died, or the peer refused the upgrade it was
    /// opened for.
    ///
    /// `only` names the camera the teardown was scheduled for, where it was
    /// scheduled at all — the work is spawned, and the camera in the registry
    /// may be a later one. `None` means "whatever is there now", which is
    /// right for something learned from the peer in the moment.
    pub(in crate::whatsapp) async fn stop_local_video(
        calls: &CallRegistry,
        ui_sender: &UiEventSender,
        call_id: &str,
        only: Option<crate::video::CameraId>,
    ) {
        // On the call's own lane, like a request, and for the same reason a
        // request is: closing a device and telling the peer are two awaits,
        // and a camera turned on between them would have its media plane
        // stopped and be published as off. The identity check below is what
        // decides whether *this* cleanup still has something to do; the lane
        // is what keeps that answer true for as long as it takes to act on
        // it. Blocking here delays this one call's events and nothing else:
        // the lane is per call, and the library's event queue is unbounded.
        let lane = calls.video_lane(call_id);
        let _serialized = lane.lane.lock().await;
        let Some(local) = calls.take_camera_if(call_id, only) else {
            return;
        };
        local.stop().await;
        // Asked for out of the registry rather than held across the wait:
        // telling the peer is a stanza on the wire, and holding the lock
        // across it stalls every other call's bookkeeping behind one peer.
        if let Some(handle) = calls.live(call_id) {
            Self::stop_peer_video(&handle, call_id).await;
        }
        Self::announce_video(ui_sender, call_id, VideoStream::Local, false).await;
    }

    /// Tell the peer this side's video has stopped, and say so if it could
    /// not be told: a direction they still believe is live is one they hold a
    /// pane open for.
    async fn stop_peer_video(handle: &CallHandle, call_id: &str) {
        if let Err(e) = handle.stop_video().await {
            warn!(
                "Failed to tell the peer video stopped on {}: {}",
                call_id, e
            );
        }
    }

    /// Drop a parked upgrade request and tell every window it is gone.
    ///
    /// Both halves or neither: the token is what an answer is bound to, and a
    /// front end still offering to answer a request the session can no longer
    /// act on would produce a camera turning on for nobody.
    async fn withdraw_video_request(
        calls: &CallRegistry,
        ui_sender: &UiEventSender,
        call_id: &str,
    ) {
        if calls.take_upgrade(call_id).is_some() {
            Self::announce_video_request(ui_sender, call_id, false).await;
        }
    }

    async fn announce_video_request(ui_sender: &UiEventSender, call_id: &str, pending: bool) {
        if let Some(tx) = ui_sender.lock().await.as_ref() {
            let _ = tx.send(UiEvent::CallVideoRequested {
                call_id: call_id.to_string(),
                pending,
            });
        }
    }

    async fn announce_video(
        ui_sender: &UiEventSender,
        call_id: &str,
        stream: VideoStream,
        on: bool,
    ) {
        if let Some(tx) = ui_sender.lock().await.as_ref() {
            let _ = tx.send(UiEvent::CallVideoChanged {
                call_id: call_id.to_string(),
                stream,
                on,
            });
        }
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
            match calls.decline(&call_id) {
                Declined::Ringing(offer) => match client.voip().reject(&offer).await {
                    Ok(()) => info!("Call {} declined", call_id),
                    Err(e) => error!("Failed to decline call {}: {}", call_id, e),
                },
                // The acceptance holds the offer and will reject it: it is
                // the only side that can, and it now knows to.
                Declined::Accepting => {
                    info!("Call {} declined while it was being answered", call_id);
                }
                // A decline that arrived after somebody answered. The card is
                // gone from every window either way, so the honest reading is
                // a hangup.
                Declined::Live(handle) => {
                    log_termination(&call_id, handle.terminate().await);
                }
                Declined::Nothing => warn!("No pending offer for call {}", call_id),
            }
        });
    }

    /// Place an outgoing 1:1 call. Device discovery, callKey encrypt, offer
    /// send and the relay/engine lifecycle are inside `client.voip().call(..)`.
    ///
    /// `is_video` reaches the wire: the offer itself says which kind of call
    /// this is, and it says so because the endpoints were attached before it
    /// went out. A video call whose camera would not open is placed as a
    /// voice call rather than not placed at all — the point of the call is to
    /// reach the person.
    pub fn start_call(&self, recipient_jid_str: &str, is_video: bool, placeholder_id: String) {
        let client_handle = self.client_handle.clone();
        let calls = self.calls.clone();
        let ui_sender = self.ui_sender.clone();
        let publish = self.video_publisher();
        let lost = self.camera_lost();
        let recipient_jid = recipient_jid_str.to_string();

        // Before the spawn, on the caller's thread, for the same reason a
        // mute request is stamped there: spawning is not sequencing. A user
        // who dials and immediately changes their mind produces two spawned
        // tasks, and the cancel's can run first — finding nothing in flight,
        // declining to leave a note, and returning `Nothing`. The placement
        // would then go on to offer the call and file it as live, ringing at
        // the far end with every window already showing it gone. Inside the
        // task this mark is too late by construction; here there is no gap
        // for a cancel to fall into, because the daemon takes the two
        // requests in order on one thread.
        calls.begin_start(&placeholder_id);

        self.exec.spawn(async move {
            let notify_failure = |error: String| {
                let ui_sender = ui_sender.clone();
                let recipient_jid = recipient_jid.clone();
                let calls = calls.clone();
                let placeholder_id = placeholder_id.clone();
                async move {
                    // A cancel may have landed for a call that will never
                    // start; the placement is over either way.
                    calls.abandon_start(&placeholder_id);
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
            let (mic, speaker, audio_facts) = match open_call_audio().await {
                Ok(audio) => audio,
                Err(err) => {
                    notify_failure(format!("audio device setup failed: {err}")).await;
                    return;
                }
            };

            // Opened under the placeholder id: the server has not named the
            // call yet, and the frames this produces are addressed to the
            // call the front end already drew.
            let video = if is_video {
                match video::open(video::slot(&placeholder_id), publish, lost).await {
                    Ok(video) => Some(video),
                    Err(err) => {
                        warn!(
                            "Placing the call to {} without video: {err}",
                            observe_str(&recipient_jid)
                        );
                        None
                    }
                }
            } else {
                None
            };
            let (local, endpoints) = match video {
                Some((local, endpoints)) => (Some(local), Some(endpoints)),
                None => (None, None),
            };
            // What the offer will say, decided by what is attached to it
            // rather than by what was asked for. Read here because the
            // endpoints are about to be handed away.
            let endpoints_attached = endpoints.is_some();

            let voip = client.voip();
            let outgoing = voip.call(&jid).audio(mic, speaker);
            let outgoing = match endpoints {
                Some(endpoints) => outgoing.video(endpoints.source, endpoints.sink),
                None => outgoing,
            };

            // Cancelled while the camera was opening — a device, and the
            // first time a permission prompt, is seconds in which the user
            // can change their mind. Checked *before* the offer goes out: the
            // peer would otherwise ring for a call that was called off, and
            // be told to stop moments later.
            // Consumed here, which is why `abandon_start` below has nothing
            // left to clear: an outgoing call that was called off has no
            // second reader for the note, unlike an acceptance whose guard
            // runs on every exit.
            if calls.ended_meanwhile(&placeholder_id).is_some() {
                info!(
                    "Outgoing call to {} cancelled while its camera opened",
                    observe_str(&recipient_jid)
                );
                if let Some(local) = local {
                    local.stop().await;
                }
                calls.abandon_start(&placeholder_id);
                return;
            }

            // See the accept path for why this is before the await and not
            // after it. The cancellation check above is the last exit, and it
            // drops the builder with the endpoints inside it.
            audio_facts.hand_to_engine();
            match outgoing.start().await {
                Ok(handle) => {
                    let call_id = handle.call_id().to_string();
                    let handle = Arc::new(handle);
                    // Cancelled while still connecting: the UI only knew the
                    // placeholder id, so the rename and the note are answered
                    // together, under one lock. As two steps there is a
                    // moment where a cancel finds no handle and this finds no
                    // note, and what is left is a call ringing at the far end
                    // that no window has ever been told the name of.
                    if !calls.finish_start(&placeholder_id, &call_id, &handle) {
                        info!("Outgoing call {} cancelled before start", call_id);
                        if let Some(local) = local {
                            local.stop().await;
                        }
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
                    if let Some(local) = local {
                        // A camera that died while the offer was going out
                        // reported its loss under the placeholder id, against
                        // a registry it was never in: nothing was torn down,
                        // and the entry made here would be the one nothing
                        // ever comes back for.
                        // The frames were being addressed to the placeholder
                        // the window drew; from here they carry the name the
                        // server gave the call. A reader keeps one decoder per
                        // call and cannot tell a rename from a different call,
                        // so it starts a fresh one — which has nothing to
                        // decode until a keyframe. This is that keyframe.
                        local.rename(&call_id);
                        local.request_keyframe();
                        // Nothing to tell the peer if it did not survive: the
                        // call is ringing, and what it was offered as is
                        // already out.
                        calls.hold_camera(&call_id, local).await;
                    }
                    Self::watch_call(handle, calls.clone(), ui_sender.clone());
                    // The rename first: everything after it is addressed by
                    // the id the server gave the call, and a front end told
                    // its camera was on under an id it has not adopted yet
                    // would drop the news.
                    if let Some(tx) = ui_sender.lock().await.as_ref() {
                        let _ = tx.send(UiEvent::OutgoingCallStarted {
                            call_id: call_id.clone(),
                            recipient_jid,
                            placeholder_id,
                            // What went out, not what was asked for: a video
                            // call whose camera would not open was placed as
                            // a voice call, and the state drawn from the
                            // request would otherwise hold video panes open
                            // on a call with no camera and write the
                            // conversation's record as a video call.
                            is_video: endpoints_attached,
                        });
                    }
                    // Not announced here: the call is ringing, and a ringing
                    // call has no live state to record a camera against. The
                    // peer's `<accept>` is the first moment there is one, and
                    // that is where it is said.
                }
                Err(e) => {
                    if let Some(local) = local {
                        local.stop().await;
                    }
                    notify_failure(e.to_string()).await;
                }
            }
        });
    }

    /// Hang up / cancel a call we started or answered.
    pub fn cancel_call(&self, call_id: &str) {
        let calls = self.calls.clone();
        let call_id = call_id.to_string();

        self.exec.spawn(async move {
            // One operation, because the three answers are decided by the
            // same state — see [`CallRegistry::cancel`].
            match calls.cancel(&call_id) {
                Cancelled::Live(handle) => {
                    log_termination(&call_id, handle.terminate().await);
                }
                Cancelled::Deferred => {
                    debug!("cancel_call: no live handle for {}, deferring", call_id);
                }
                Cancelled::Nothing => {}
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
            let lane = calls.mute_lane(&call_id);
            let mut intent = lane.intent.lock().expect("mute intent poisoned");
            intent.seq += 1;
            intent.muted = muted;
            let seq = intent.seq;
            drop(intent);
            (lane, seq)
        };

        self.exec.spawn(async move {
            // Taken out rather than held: `set_muted` waits on the call's
            // answer-transition lane, and holding the registry across that
            // would stall every other call's bookkeeping behind one peer.
            let handle = match calls.live_and_sweep(&call_id) {
                Some(handle) => handle,
                None => {
                    debug!("set_call_muted: no live handle for {}", call_id);
                    // Answered rather than dropped, which is the video twin's
                    // rule and holds for the same reason: the window draws
                    // the microphone as muted the moment it is asked, so a
                    // request landing between the state saying a call is live
                    // and this side registering its handle would leave that
                    // drawn over a device nothing here holds. There is no
                    // handle, so nothing is muted, and that is what is said.
                    Self::settle_mute(&ui_sender, &call_id, seq, &lane, false).await;
                    return;
                }
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
            Self::settle_mute(&ui_sender, &call_id, seq, &lane, handle.is_muted()).await;
        });
    }

    /// Publish what the microphone *is*, once the newest request has reached
    /// it. The mute half of [`Self::settle_video`], down to being silent when
    /// a newer request has arrived meanwhile.
    async fn settle_mute(
        ui_sender: &UiEventSender,
        call_id: &str,
        seq: u64,
        lane: &MuteLane,
        settled: bool,
    ) {
        if lane.intent.lock().expect("mute intent poisoned").seq != seq {
            return;
        }
        if let Some(tx) = ui_sender.lock().await.as_ref() {
            let _ = tx.send(UiEvent::CallMuteChanged {
                call_id: call_id.to_string(),
                muted: settled,
            });
        }
    }

    /// Follow a live call: its own event stream while it runs, and its
    /// ending.
    ///
    /// One entry point rather than two spawns at every call site, because
    /// every path that produces a handle owes both — a call watched for its
    /// ending but not its events is one whose camera nobody turns off.
    fn watch_call(handle: Arc<CallHandle>, calls: CallRegistry, ui_sender: UiEventSender) {
        Self::watch_call_events(handle.clone(), calls.clone(), ui_sender.clone());
        Self::watch_call_end(handle, calls, ui_sender);
    }

    /// The call's own event stream: what the peer says about its video, and
    /// what the network says about ours.
    fn watch_call_events(handle: Arc<CallHandle>, calls: CallRegistry, ui_sender: UiEventSender) {
        crate::exec::spawn(async move {
            let events = handle.events();
            let call_id = handle.call_id().to_string();
            while let Ok(event) = events.recv().await {
                match event {
                    CallEvent::VideoStateChanged {
                        state,
                        upgrade_token,
                        ..
                    } => {
                        Self::observe_peer_video(
                            &calls,
                            &ui_sender,
                            &call_id,
                            state,
                            upgrade_token,
                        )
                        .await;
                    }
                    // The peer has lost our stream and is asking for a point
                    // it can start from. Sending it more P-frames it cannot
                    // decode is the one thing that certainly does not help.
                    CallEvent::RtcpReceived {
                        reports_video,
                        feedback,
                        ..
                    } if reports_video && feedback.iter().any(reports_loss) => {
                        calls.ask_for_keyframe(&call_id);
                    }
                    // The reason the call is about to end, and the only
                    // place it is ever said. `wait_ended` fires right behind
                    // this, so a call whose media never came up otherwise
                    // vanishes a moment after it was placed with nothing
                    // anywhere saying why — which is exactly how a browser
                    // call that dials no relay reads in a console: an offer,
                    // then an ending, and not one line between them.
                    CallEvent::MediaSetupFailed(reason) => {
                        warn!("call {call_id} media setup failed: {reason}");
                        if let Some(tx) = ui_sender.lock().await.as_ref() {
                            let _ = tx.send(UiEvent::CallMediaFailed {
                                call_id: call_id.clone(),
                                reason,
                            });
                        }
                    }
                    _ => {}
                }
            }
            debug!("event stream for call {call_id} closed");
        });
    }

    /// Fold one `<video state=N>` from the peer into what this side holds.
    async fn observe_peer_video(
        calls: &CallRegistry,
        ui_sender: &UiEventSender,
        call_id: &str,
        state: VideoState,
        upgrade_token: Option<VideoUpgradeToken>,
    ) {
        if peer_can_receive_video(state) {
            calls.ask_for_keyframe(call_id);
        }
        match state {
            // A request rather than a change: the answer is a person turning
            // their own camera on, and the token is what binds that answer to
            // this request. Without a token there is nothing to answer with —
            // the signaling state machine has already resolved it — so it is
            // not offered as a question.
            VideoState::UpgradeRequest | VideoState::UpgradeRequestV2 => {
                let Some(token) = upgrade_token else { return };
                calls.park_upgrade(call_id, token);
                Self::announce_video_request(ui_sender, call_id, true).await;
            }
            // Their camera, not our answer. `Enabled` is what a peer sends
            // for their own camera and their own rotations, so retiring our
            // outstanding question on it leaves a later refusal answering
            // nothing here while the library has already released the plane
            // under it: the device stays open, encoding for nobody, drawn as
            // live. Only the two stanzas that are addressed to our request
            // retire it.
            VideoState::Enabled => {
                Self::announce_video(ui_sender, call_id, VideoStream::Remote, true).await;
            }
            // Paused is drawn the same as off, and deliberately: a peer whose
            // app went to the background sends nothing, and a pane held open
            // for it would be a frozen frame nobody can tell from a live one.
            VideoState::Stopped
            | VideoState::Disabled
            | VideoState::Paused
            | VideoState::Error
            | VideoState::UnknownPeer => {
                Self::withdraw_video_request(calls, ui_sender, call_id).await;
                Self::announce_video(ui_sender, call_id, VideoStream::Remote, false).await;
            }
            // Our own upgrade was refused, or ran out of time waiting to be
            // answered. The camera was opened and announced when the request
            // went out — the library holds it off the wire until the peer
            // accepts — so a refusal that only closed the question would
            // leave the device open and encoding for the rest of the call,
            // with every window saying our video was on.
            VideoState::UpgradeReject | VideoState::UpgradeRejectByTimeout => {
                Self::withdraw_video_request(calls, ui_sender, call_id).await;
                // Only while a request of ours is outstanding, and then for
                // whatever camera is held — which is what the library does
                // with the same stanza: it tears the attached plane down when
                // some request of ours is pending, and ignores the refusal
                // when none is. One arriving after our upgrade was already
                // answered belongs to nothing here, and stopping the camera
                // on it would take down one nobody refused.
                if calls.end_upgrade(call_id) {
                    Self::stop_local_video(calls, ui_sender, call_id, None).await;
                } else {
                    debug!("Refused video upgrade on {call_id} answers nothing of ours");
                }
            }
            // Their request, withdrawn or timed out. Nothing about either
            // camera changed; what has changed is that there is no longer a
            // question on the table, and a front end still drawing one would
            // be pointing at a peer who has stopped waiting.
            VideoState::UpgradeCancel | VideoState::UpgradeCancelByTimeout => {
                Self::withdraw_video_request(calls, ui_sender, call_id).await;
            }
            // The peer took the upgrade, so nothing of ours is outstanding
            // and a refusal landing after it answers something else.
            VideoState::UpgradeAccept => {
                calls.end_upgrade(call_id);
            }
            // `UpgradeAccept` is answered by the `Enabled` that follows it,
            // which is the state that actually says a camera is on.
            _ => {}
        }
    }

    /// Watch a live call until it ends (peer hangup, network loss, local
    /// hangup) and clear it from the registry + UI.
    fn watch_call_end(handle: Arc<CallHandle>, calls: CallRegistry, ui_sender: UiEventSender) {
        crate::exec::spawn(async move {
            handle.wait_ended().await;
            let call_id = handle.call_id().to_string();
            if let Some(camera) = calls.ended(&call_id) {
                camera.stop().await;
            }
            Self::notify_call_ended(&ui_sender, &call_id).await;
        });
    }

    async fn notify_call_ended(ui_sender: &UiEventSender, call_id: &str) {
        if let Some(tx) = ui_sender.lock().await.as_ref() {
            let _ = tx.send(UiEvent::CallEnded(call_id.to_string()));
        }
    }
}

/// RTCP payload-specific feedback (RFC 4585), which is the *class* PLI and
/// FIR belong to.
const RTCP_PAYLOAD_FEEDBACK: u8 = 206;
/// Picture Loss Indication: the peer cannot decode what we are sending.
const RTCP_FMT_PLI: u8 = 1;
/// Full Intra Request, which asks for the same thing more emphatically.
const RTCP_FMT_FIR: u8 = 4;

/// Whether one feedback message says the peer has lost our picture.
///
/// The packet type alone does not: 206 also carries REMB bandwidth estimates
/// and other formats a healthy call sends continuously, and treating those as
/// loss would emit a keyframe at the RTCP reporting rate — large frames, over
/// and over, defeating the very bitrate control they are reported against.
fn reports_loss(feedback: &whatsapp_rust::wacore::voip::rtcp::RtcpFeedback) -> bool {
    feedback.packet_type == RTCP_PAYLOAD_FEEDBACK
        && matches!(feedback.fmt, RTCP_FMT_PLI | RTCP_FMT_FIR)
}

/// Whether this peer state is a decoder of theirs being born on *our* stream.
///
/// The same rule the window's own decoders follow, applied to the one on the
/// far side: an upgrade holds our video off the wire until the peer accepts,
/// so the accept is the first moment they have anywhere to put it — and what
/// they receive from there references units encoded while nobody was
/// listening. Our encoder emits an IDR every few seconds on its own, so
/// without this the picture arrives when it arrives, which is a peer looking
/// at a blank pane for up to a GOP after answering.
///
/// `Enabled` as well as `UpgradeAccept`, because either can be the stanza that
/// ungates us: the library takes an `Enabled` from a peer who skipped the
/// accept as the answer to our request. It is also what a peer sends for their
/// own camera and their own rotations, so this asks for a keyframe more often
/// than strictly needed — one extra frame against a picture that never starts.
fn peer_can_receive_video(state: VideoState) -> bool {
    matches!(state, VideoState::UpgradeAccept | VideoState::Enabled)
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

    /// A peer turning their own camera on is not an answer to ours. Taken as
    /// one, the refusal that follows finds nothing outstanding, leaves
    /// `stop_local_video` unrun, and the camera stands open with its light on
    /// over a media plane the library has already released.
    #[tokio::test]
    async fn a_peers_own_camera_does_not_answer_our_upgrade() {
        let calls = CallRegistry::default();
        let (tx, _rx) = mpsc::unbounded_channel();
        let ui: UiEventSender = Arc::new(Mutex::new(Some(tx)));

        calls.begin_upgrade("call-1");
        WhatsAppClient::observe_peer_video(&calls, &ui, "call-1", VideoState::Enabled, None).await;

        assert!(
            calls.end_upgrade("call-1"),
            "our upgrade is still waiting on an answer of its own"
        );
    }

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

    /// A request that finds no handle used to return without a word, so the
    /// window went on drawing the microphone it had drawn optimistically.
    /// The video twin answers with what the registry holds; this one answers
    /// with the only thing true of a call with no handle.
    #[tokio::test]
    async fn a_mute_request_with_no_handle_still_says_what_the_microphone_is() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ui_sender: UiEventSender = Arc::new(Mutex::new(Some(tx)));
        let lane = MuteLane::default();
        let seq = request(&lane, true);

        WhatsAppClient::settle_mute(&ui_sender, "call-1", seq, &lane, false).await;

        match rx.try_recv().expect("the request is answered") {
            UiEvent::CallMuteChanged { call_id, muted } => {
                assert_eq!(call_id, "call-1");
                assert!(!muted, "nothing holds the device, so nothing is muted");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// The window between an offer leaving `pending` and a handle reaching
    /// `active` is a real one — opening the audio devices and connecting the
    /// relay both take time — and a `<terminate>` landing inside it used to
    /// find the call in neither collection, remove nothing, and let the
    /// acceptance file a live handle behind the `CallEnded` the window had
    /// already been sent. A microphone open under a call nobody thought was
    /// happening.
    #[test]
    fn a_peer_ending_a_call_mid_acceptance_is_not_lost() {
        let calls = CallRegistry::default();
        calls.mark_accepting("call-1");
        calls.ended_remotely("call-1");

        assert!(
            calls.abandon_accept("call-1"),
            "an ending that arrived mid-acceptance must reach the acceptance"
        );
    }

    /// And it is spent once, so a later acceptance of a call with a reused id
    /// is not cancelled by a stale note.
    #[test]
    fn a_peer_ending_is_reported_once() {
        let calls = CallRegistry::default();
        calls.mark_accepting("call-1");
        calls.ended_remotely("call-1");

        assert!(calls.abandon_accept("call-1"));
        assert!(
            !calls.abandon_accept("call-1"),
            "the note is consumed by the acceptance that acted on it"
        );
    }

    /// A decline that lands after the accept has taken the offer still stops
    /// the call.
    ///
    /// Two windows on one account: one answers, the other declines before it
    /// has seen the call go live. Reading `pending` alone found nothing there
    /// — the acceptance had it — and the accept went on to register a live
    /// handle behind a card every window had already cleared: audible call,
    /// no controls.
    #[test]
    fn a_decline_reaches_an_acceptance_already_in_flight() {
        let calls = CallRegistry::default();
        calls.mark_accepting("call-1");

        assert!(matches!(calls.decline("call-1"), Declined::Accepting));
        assert_eq!(
            calls.ending_for("call-1"),
            Some(Ending::Local),
            "the acceptance holds the offer, so the rejection is its to send"
        );
    }

    /// And one with nothing at any stage is nothing, rather than a note that
    /// outlives every call that merely rang.
    #[test]
    fn a_decline_for_a_call_nobody_has_records_nothing() {
        let calls = CallRegistry::default();
        assert!(matches!(calls.decline("call-1"), Declined::Nothing));
        assert_eq!(calls.ending_for("call-1"), None);
    }

    /// Which side ended it survives the wait, because the two endings are
    /// different words on the wire: a call the peer ended needs no
    /// `<terminate>` back, and one we ended has devices still ringing.
    #[test]
    fn an_ending_remembers_whose_it_was() {
        let ours = CallRegistry::default();
        ours.mark_accepting("call-1");
        assert!(matches!(ours.cancel("call-1"), Cancelled::Deferred));
        assert_eq!(ours.ending_for("call-1"), Some(Ending::Local));

        let theirs = CallRegistry::default();
        theirs.mark_accepting("call-2");
        theirs.ended_remotely("call-2");
        assert_eq!(theirs.ending_for("call-2"), Some(Ending::Remote));
    }

    /// Nothing is recorded when no acceptance is in flight, or the set would
    /// grow by one entry for every call that simply rang and stopped.
    #[test]
    fn an_ending_with_nothing_in_flight_records_nothing() {
        let calls = CallRegistry::default();
        calls.ended_remotely("call-1");

        calls.mark_accepting("call-1");
        assert!(
            !calls.abandon_accept("call-1"),
            "an ending before the acceptance began is not this acceptance's"
        );
    }

    /// The guard clears the mark as it drops, so a decline arriving after
    /// the acceptance ended has an offer to reject rather than being told an
    /// acceptance will do it.
    #[test]
    fn a_decline_after_the_acceptance_ended_is_not_left_to_the_acceptance() {
        let calls = CallRegistry::default();
        calls.mark_accepting("call-1");
        drop(AcceptGuard {
            calls: calls.clone(),
            call_id: "call-1".to_string(),
        });

        assert!(
            matches!(calls.decline("call-1"), Declined::Nothing),
            "the acceptance is over, so nothing is going to send the rejection"
        );
        assert_eq!(calls.ending_for("call-1"), None, "and nothing is left over");
    }

    /// A cancel for a call that is still connecting has to survive until the
    /// handle exists.
    ///
    /// The window only ever knew the placeholder id, so that is what it
    /// cancels under. Answering the cancel separately from filing the handle
    /// left a gap where a start consumed the note before it was written —
    /// and the abandoned attempt then rang at the far end until its transport
    /// gave up.
    #[test]
    fn a_cancel_while_connecting_reaches_the_start() {
        let calls = CallRegistry::default();
        calls.begin_start("placeholder-1");

        assert!(
            matches!(calls.cancel("placeholder-1"), Cancelled::Deferred),
            "nothing is live yet, so the cancel is left for the start"
        );
    }

    /// And a cancel with nothing in flight is not remembered, for the same
    /// reason an ending is not — which is also why `start_call` marks the
    /// placeholder on the caller's thread rather than inside its task. This
    /// answer is right for the registry and fatal as a sequence: reached
    /// because a cancel's task ran before a placement's, it would let the
    /// placement go on to offer a call every window had already cleared.
    #[test]
    fn a_cancel_with_nothing_in_flight_is_not_remembered() {
        let calls = CallRegistry::default();
        assert!(matches!(calls.cancel("placeholder-1"), Cancelled::Nothing));
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

    /// A keyframe is asked for when the peer says it lost the picture, and
    /// not when it says anything else on the same channel.
    ///
    /// Payload-specific feedback (206) is a *class*: REMB bandwidth estimates
    /// ride it too, continuously, on a call that is going perfectly well.
    /// Treating the class as loss emits an IDR at the reporting rate — large
    /// frames, over and over, against the very bitrate those reports exist to
    /// manage.
    #[test]
    fn only_a_lost_picture_asks_for_a_keyframe() {
        use whatsapp_rust::wacore::voip::rtcp::RtcpFeedback;

        let feedback = |packet_type, fmt| RtcpFeedback {
            packet_type,
            fmt,
            sender_ssrc: 1,
            media_ssrc: 2,
            fci: Vec::new(),
        };
        // Picture Loss Indication and Full Intra Request.
        assert!(reports_loss(&feedback(206, 1)));
        assert!(reports_loss(&feedback(206, 4)));
        // REMB is 206/15, and a healthy call sends it forever.
        assert!(!reports_loss(&feedback(206, 15)));
        assert!(!reports_loss(&feedback(206, 3)));
        // Transport feedback (205) carries its own format 1, which is a NACK
        // and not a request to start over.
        assert!(!reports_loss(&feedback(205, 1)));
        assert!(!reports_loss(&feedback(200, 4)));
    }

    /// The states that mean the peer now has somewhere to put our video, and
    /// so has a decoder that has never seen a keyframe.
    ///
    /// An upgrade we initiated is held off the wire until they accept, so
    /// everything encoded before that accept is a reference they do not have.
    #[test]
    fn a_peer_that_can_receive_is_asked_for_a_fresh_start() {
        assert!(peer_can_receive_video(VideoState::UpgradeAccept));
        assert!(peer_can_receive_video(VideoState::Enabled));
        // Nothing on the far side is waiting for a picture in any of these.
        for quiet in [
            VideoState::Stopped,
            VideoState::Disabled,
            VideoState::Paused,
            VideoState::UpgradeRequest,
            VideoState::UpgradeRequestV2,
            VideoState::UpgradeReject,
            VideoState::UpgradeRejectByTimeout,
            VideoState::UpgradeCancel,
            VideoState::UpgradeCancelByTimeout,
            VideoState::UnknownPeer,
            VideoState::Error,
        ] {
            assert!(!peer_can_receive_video(quiet), "{quiet:?}");
        }
    }
}
