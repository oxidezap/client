//! What this front end cannot do, asked before it is offered.
//!
//! The twin of [`super::plugins`], and the same shape: each answer is either
//! `None` or the sentence to draw instead. What they have in common is being
//! questions about the *platform and the session* rather than about a
//! particular file or a particular moment, which is what makes them safe to
//! ask early — and asking early is the whole value, since the alternative in
//! both cases is a control that is offered, acted on, and then fails the same
//! way every time.
//!
//! # Sending media, and why it is not asked about here
//!
//! It was, and it should not have been. This module used to withhold the
//! microphone from a page holding its own session, on the ground that the
//! library's upload went through `execute_upload` — which a browser cannot
//! implement, being synchronous — so every media send from such a page would
//! fail at the same place however many times it was tried.
//!
//! That is not what the library does, and at the revision the lockfile names
//! it never was: `execute_upload` is the *streaming* path
//! (`Client::upload_stream`), and `Client::upload` reaches the CDN through
//! `HttpClient::execute` with a body on it — which is exactly the one method
//! `BrowserHttpClient` implements. So both halves of the journey exist on
//! both platforms, and the question is gone rather than answered `None`
//! everywhere: a capability that is never missing is not a capability, and
//! one kept "just in case" is a branch nobody can test.
//!
//! The lesson is about the rest of this file rather than about this entry. An
//! answer here is a claim about somebody else's code, and this one was
//! written from a reading and never re-read against the revision it was
//! about — for long enough that a working feature was withheld by it.
//!
//! What remains true is the *cost*: a page has one thread, so encrypting a
//! large file happens on it. That is a delay, not an impossibility, and the
//! send reports its own failures.

/// # Decoding video
///
/// Why this front end cannot decode a video at all, or `None` if it can.
///
/// Asked *before* the attachment is fetched, which is the whole point: the
/// decoder is built from the parameter sets, so the first thing that knows
/// this browser has no `VideoDecoder` is a call made after the whole file has
/// been downloaded and demuxed. The bubble then draws that failure as Retry,
/// and every retry pays the download again to reach the same permanent
/// answer.
///
/// A capability rather than a build, for the reason `can_record` became a
/// function: WebCodecs is something a browser either has or does not, and an
/// older one is the case this exists for.
#[must_use]
pub fn video_decode_unavailable() -> Option<&'static str> {
    imp::video_decode_unavailable()
}

/// # A call from the wrong tab
///
/// Why a call cannot be *started here*, though this front end could carry
/// one, or `None` if this is the right window to start it in.
///
/// Asked separately from [`calls_unavailable`] and never folded into it,
/// because the two want opposite things done about a ringing call. A window
/// that cannot carry a call at all owes the caller an answer, so it declines.
/// A window that is merely the wrong one owes them nothing: the call is
/// perfectly answerable in the tab beside it, and declining here would send
/// `Decline` to the leader and clear the offer *everywhere* — telling somebody
/// to use the other tab while destroying the call they would have used it for.
///
/// So this one leaves the offer ringing and says where to answer it.
#[must_use]
pub fn calls_belong_to_another_tab() -> Option<&'static str> {
    imp::calls_belong_to_another_tab()
}

/// # Placing a call
///
/// Why this front end cannot carry a call's media, or `None` if it can.
///
/// A call's media rides a pre-negotiated WebRTC DataChannel, which on a
/// desktop the daemon builds over a UDP socket and in a page is an
/// `RTCPeerConnection`. A browser old enough to lack one cannot carry a call
/// at all, and that is worth knowing before somebody presses Call and grants
/// microphone permission to something that was never going to connect --
/// which is the same sentence this module makes about the microphone and
/// about `VideoDecoder`.
///
/// Not asked about the *camera* separately, and deliberately: one that will
/// not open downgrades a call to voice rather than failing it, on both
/// platforms, so a browser with `RTCPeerConnection` and no `VideoEncoder`
/// places an ordinary voice call. There is nothing to withhold.
///
/// The *decoder* is a different question and is asked separately, by the
/// caller, through [`video_decode_unavailable`]. It belongs to this front end
/// whoever holds the session — a daemon can open its camera, negotiate video
/// and send perfectly while a window without `VideoDecoder` rejects every
/// access unit — so it is not a reason a call cannot be carried, and it is a
/// reason a call should not be *placed as video*.
///
/// The GUI has no route to `oxidezap-session`'s
/// `relay::sdp::RELAY_DTLS_FINGERPRINT` -- it depends on ipc, core and audio,
/// never on the session -- so this does not read that constant. It does not
/// need to: that constant is filled in now, so what is left to ask is what
/// this browser has.
#[must_use]
pub fn calls_unavailable() -> Option<&'static str> {
    imp::calls_unavailable()
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    /// openh264 is linked in.
    pub(super) fn video_decode_unavailable() -> Option<&'static str> {
        None
    }

    /// The daemon holds the session, and with it the UDP socket.
    pub(super) fn calls_unavailable() -> Option<&'static str> {
        None
    }

    /// There are no follower windows here: every one talks to the daemon.
    pub(super) fn calls_belong_to_another_tab() -> Option<&'static str> {
        None
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    /// Asked of the global rather than by constructing one: a `VideoDecoder`
    /// needs a configuration to be built and there is none to give before the
    /// file is here, which is precisely the ordering this exists to fix.
    pub(super) fn video_decode_unavailable() -> Option<&'static str> {
        let global = js_sys::global();
        match js_sys::Reflect::get(&global, &wasm_bindgen::JsValue::from_str("VideoDecoder")) {
            Ok(found) if !found.is_undefined() && !found.is_null() => None,
            _ => Some("Videos cannot be played in this browser."),
        }
    }

    /// Asked of the *session* first, not of the build — which this answered
    /// wrongly at first by looking only at the page.
    ///
    /// A page attached to a real daemon does not place its call in the
    /// browser at all: the daemon holds the session, and with it the UDP
    /// socket and the native relay dialler. Whether *this* browser has
    /// `RTCPeerConnection` is beside the point there, and refusing on it
    /// blocks a call that would have worked.
    ///
    /// A page holding its own session is the case the browser relay exists
    /// for, and there the question is the browser's: the media rides a
    /// pre-negotiated DataChannel over an `RTCPeerConnection`, so an agent
    /// without one carries nothing. Asked before the control is drawn,
    /// because a browser is not going to grow one between the question and
    /// the press — the same rule the rest of this module follows.
    pub(super) fn calls_unavailable() -> Option<&'static str> {
        match oxidezap_ipc::web::named_daemon() {
            oxidezap_ipc::web::NamedDaemon::Named(_) => None,
            // `Rejected` answered like `Nobody`, as above: the window is on
            // the refusal screen and drawing no call control either way.
            _ if has_peer_connection() => None,
            _ => Some(
                "This browser has no RTCPeerConnection, which is what carries \
                 a call's media here.",
            ),
        }
    }

    /// A follower tab holds no session, so its Place or Accept is executed by
    /// the tab that does — and that tab is where `getUserMedia` and
    /// `AudioContext::resume` would run. The devices would be the *leader's*:
    /// its microphone, its speakers, its permission prompt, in a document the
    /// person pressing the button is not looking at and has not gestured in.
    /// The prompt is the half that might work by luck, since permission is
    /// per-origin; the speakers are the half that cannot, and a call heard in
    /// a tab nobody is using is not a call.
    ///
    /// This is the one place a follower differs from a page attached to an
    /// `oxidezapd`, which is allowed — and the contrast is what makes the
    /// distinction right rather than arbitrary: there the devices are the
    /// daemon's *by design* and nobody expects the window to hold them, while
    /// here both tabs are windows and the wrong one would.
    ///
    /// Not a capability this window is missing, which is why it is not
    /// `calls_unavailable`: it is which document owns the media, and moving
    /// that means the follower opening the devices and handing them across —
    /// a change to the tab protocol rather than a check.
    pub(super) fn calls_belong_to_another_tab() -> Option<&'static str> {
        if matches!(
            oxidezap_ipc::web::named_daemon(),
            oxidezap_ipc::web::NamedDaemon::Named(_)
        ) || crate::session::this_tab_holds_the_account()
        {
            return None;
        }
        Some(
            "This tab is showing an account another tab is running, so a call \
             would use that tab's microphone and speakers. Use that tab.",
        )
    }

    /// Whether the agent this page runs in defines `RTCPeerConnection`.
    ///
    /// The twin of `oxidezap-session`'s own check, which this crate cannot
    /// reach — it never depends on the session — so it is asked of the global
    /// the same way rather than through a `web_sys` binding: the binding
    /// exists whether the browser does or not.
    fn has_peer_connection() -> bool {
        let global = js_sys::global();
        js_sys::Reflect::get(
            &global,
            &wasm_bindgen::JsValue::from_str("RTCPeerConnection"),
        )
        .is_ok_and(|v| !v.is_undefined() && !v.is_null())
    }
}
