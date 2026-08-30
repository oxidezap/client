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
//! # Sending media
//!
//! Not "is the network up", which a failed send already reports. A page that
//! holds its own session cannot upload a payload *at all*: the library's
//! `upload_media_with_retry` calls `execute_upload` unconditionally, and a
//! browser cannot implement it, so every media send from such a page fails at
//! the same place for the same reason however many times it is tried.
//!
//! Which makes it a question worth asking before the microphone is offered,
//! rather than after a voice note has been recorded. The composer already
//! draws the microphone disabled where the *browser* has no Opus encoder, on
//! the stated ground that a control which is drawn and then always fails is
//! worse than one that is not drawn. This is the same sentence about the
//! other half of the journey.
//!
//! The upstream half is in `whatsapp-rust`, and when it grows a buffered
//! fallback this answers `None` everywhere and the control comes back.

/// Why this front end cannot send media, or `None` if it can.
///
/// A sentence, because it is drawn as one.
#[must_use]
pub fn media_send_unavailable() -> Option<&'static str> {
    imp::media_send_unavailable()
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    /// A desktop front end hands its media to `oxidezapd`, which holds the
    /// ureq client and does the upload.
    pub fn media_send_unavailable() -> Option<&'static str> {
        None
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    /// A page attached to a real daemon sends media through it: the payload
    /// is staged over the bridge and the daemon's own HTTP client uploads it.
    /// It is only a page holding the session *itself* that cannot, and it is
    /// asked the same way the session picks which of the two it is, so the
    /// two cannot answer differently.
    pub fn media_send_unavailable() -> Option<&'static str> {
        match oxidezap_ipc::web::named_daemon() {
            oxidezap_ipc::web::NamedDaemon::Named(_) => None,
            // `Rejected` is not "no daemon", but the window is on the settled
            // refusal screen and drawing no composer at all, so it is
            // answered with the same sentence rather than a third case
            // nothing can reach.
            _ => Some(
                "A page that holds its own account cannot upload media yet: \
                 the library's upload path has no route a browser can take. \
                 Point this page at an oxidezapd with #daemon=ws://… and it \
                 sends through that.",
            ),
        }
    }
}

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
    video::video_decode_unavailable()
}

#[cfg(not(target_family = "wasm"))]
mod video {
    /// openh264 is linked in.
    pub fn video_decode_unavailable() -> Option<&'static str> {
        None
    }
}

#[cfg(target_family = "wasm")]
mod video {
    /// Asked of the global rather than by constructing one: a `VideoDecoder`
    /// needs a configuration to be built and there is none to give before the
    /// file is here, which is precisely the ordering this exists to fix.
    pub fn video_decode_unavailable() -> Option<&'static str> {
        let global = js_sys::global();
        match js_sys::Reflect::get(&global, &wasm_bindgen::JsValue::from_str("VideoDecoder")) {
            Ok(found) if !found.is_undefined() && !found.is_null() => None,
            _ => Some("Videos cannot be played in this browser."),
        }
    }
}
