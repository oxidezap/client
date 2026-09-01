//! Media, over HTTP, because the two ends share no filesystem.
//!
//! A frame names its media by a key rather than carrying it, and natively
//! both ends open the same file. A page has neither that file nor a directory
//! to look for it in, so the three things a front end does with a key —
//! read one payload, stage one it is about to send, drop one whose send is
//! not going to happen — are requests to the daemon's bridge instead.
//!
//! Not a transport, which is why it is beside [`super::socket`] rather than
//! inside it: the port is shared, and nothing else is. Every request here
//! carries the same token the socket presents, because a photo is as much the
//! account's as a frame is — a request without it is answered `404` and the
//! photo draws as a download nobody asked for.
//!
//! Each one is also under a deadline. A `fetch` against a bridge that
//! accepted the connection and then said nothing never settles on its own,
//! and the caller resolves a frame's media before it hands the frame on — so
//! an unbounded request is a stalled frame rather than a slow one.

use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;

use super::address::endpoint_url;

/// How long one media payload may take before it is treated as unavailable.
///
/// Generous: the bridge is normally on the same machine, and this exists to
/// bound a hang rather than to police a slow link.
const MEDIA_TIMEOUT_MS: i32 = 30_000;

/// How long staging a payload may take.
///
/// Longer than a read, because this one has no second chance: media the page
/// failed to fetch is drawn as an offer to download again, and a recording
/// that fails to stage is a message the person already watched themselves
/// send.
const UPLOAD_TIMEOUT_MS: i32 = 60_000;

/// How long a discard may take before it is given up on.
///
/// Far shorter than an upload, because this carries no body: it is one
/// request naming a key the daemon already holds. What is behind it is a send
/// that has already failed, so waiting a minute for the cleanup buys nothing.
const DISCARD_TIMEOUT_MS: i32 = 10_000;

/// A `setTimeout` that aborts a fetch, cleared when the fetch finishes first.
struct FetchDeadline {
    handle: i32,
    _fire: Closure<dyn FnMut()>,
}

impl FetchDeadline {
    fn arm(
        window: &web_sys::Window,
        abort: &web_sys::AbortController,
        millis: i32,
    ) -> Result<Self, String> {
        let abort = abort.clone();
        let fire = Closure::<dyn FnMut()>::new(move || abort.abort());
        let handle = window
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                fire.as_ref().unchecked_ref(),
                millis,
            )
            .map_err(|e| format!("could not arm a fetch timeout: {e:?}"))?;
        Ok(Self {
            handle,
            _fire: fire,
        })
    }
}

impl Drop for FetchDeadline {
    fn drop(&mut self) {
        if let Some(window) = web_sys::window() {
            window.clear_timeout_with_handle(self.handle);
        }
    }
}

/// Where the media this daemon has cached can be fetched from.
///
/// The same origin as the socket, over HTTP: media never travels as a frame
/// (see [`crate::media_path`]), and where the two processes share no
/// filesystem the bytes have to come from somewhere. The daemon's web bridge
/// serves them beside the socket, so deriving one from the other is what
/// keeps a page from needing to be told twice.
#[must_use]
pub fn media_base_url() -> String {
    let socket = endpoint_url();
    // Through the parser rather than by trimming a suffix off the string. A
    // socket URL is allowed a query — the bridge routes on the path alone, so
    // `ws://host/ws?token=x` connects — and a suffix test then finds no `/ws`
    // to remove and produces `http://host/ws?token=x/media`, which asks the
    // socket endpoint for every photo.
    let Ok(parsed) = web_sys::Url::new(&socket) else {
        // `endpoint_url` returns only what already parsed, or the built-in
        // default; this is unreachable rather than a fallback.
        return format!(
            "http://127.0.0.1:{}{}",
            crate::DEFAULT_WEB_PORT,
            crate::WEB_MEDIA_PATH
        );
    };
    parsed.set_protocol(if parsed.protocol() == "wss:" {
        "https:"
    } else {
        "http:"
    });
    parsed.set_pathname(crate::WEB_MEDIA_PATH);
    // No query and no fragment: the key is joined onto this, so anything
    // here would land in the middle of the path. The token the media endpoint
    // also requires is appended after the key instead — see
    // [`media_token`].
    parsed.set_search("");
    parsed.set_hash("");
    // No trailing slash: `fetch_media` joins the key with one.
    parsed.href().trim_end_matches('/').to_string()
}

/// The token the media endpoint requires, as a query ready to append.
///
/// It is the *daemon's* token rather than the socket's, and the media
/// endpoint is behind the same check — so a request without it is a `404`
/// and every photo draws as a download nobody asked for. Empty when the page
/// was pointed at a daemon without one, which is a daemon that will refuse
/// the socket too: the failure belongs there, said once, rather than here per
/// photo.
#[must_use]
pub fn media_token() -> String {
    let Ok(parsed) = web_sys::Url::new(&endpoint_url()) else {
        return String::new();
    };
    parameter_of(&parsed.search(), "token")
        .map_or_else(String::new, |token| format!("?token={token}"))
}

/// One query parameter out of a query string.
///
/// The value is left encoded: it goes straight back into a URL.
fn parameter_of(search: &str, name: &str) -> Option<String> {
    search.trim_start_matches('?').split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.to_string())
    })
}

/// Fetch one cached media payload from the daemon's bridge.
///
/// The web half of what `std::fs::read(media_path(key))` does natively. Same
/// key, same bytes, one HTTP round trip instead of a file read — which is
/// also why it is `async` where the native one is not, and why the front end
/// resolves media before it hands a frame on rather than inside it.
///
/// # Errors
///
/// A key the daemon does not hold, or a bridge that is not answering.
pub async fn fetch_media(base: &str, key: &str) -> Result<Vec<u8>, String> {
    fetch_media_within(base, key, MEDIA_TIMEOUT_MS, u64::MAX).await
}

/// Hand the daemon a payload it is about to be asked to send.
///
/// The mirror of [`fetch_media`], and the only direction a page writes. A
/// voice note exists only in the tab's memory until this lands, and the
/// request naming the key must not go out before it does, so the caller waits
/// on this rather than firing it alongside.
///
/// `PUT` because the key names the payload and staging it twice is the same
/// act twice. The bridge takes only `u-` keys, so this cannot reach the
/// daemon's own cache of what it fetched.
///
/// # Errors
///
/// The browser refused the request, the bridge refused the payload, or the
/// deadline passed.
pub async fn upload_media(base: &str, key: &str, bytes: &[u8]) -> Result<(), String> {
    use wasm_bindgen_futures::JsFuture;

    let window = web_sys::window().ok_or("no window to upload from")?;
    let url = format!(
        "{base}/{}{}",
        js_sys::encode_uri_component(key),
        media_token()
    );

    // Copied into a JS array rather than viewed: a view over wasm memory is
    // invalidated by any allocation the fetch machinery makes, and the body
    // outlives this call.
    let body = js_sys::Uint8Array::from(bytes);

    let abort = web_sys::AbortController::new()
        .map_err(|e| format!("could not arm an upload timeout: {e:?}"))?;
    let options = web_sys::RequestInit::new();
    options.set_method("PUT");
    options.set_body(&body);
    options.set_signal(Some(&abort.signal()));
    let _timeout = FetchDeadline::arm(&window, &abort, UPLOAD_TIMEOUT_MS)?;

    let response = JsFuture::from(window.fetch_with_str_and_init(&url, &options))
        .await
        .map_err(|e| format!("could not reach the daemon's media bridge: {e:?}"))?
        .dyn_into::<web_sys::Response>()
        .map_err(|_| {
            "the media bridge answered with something that is not a response".to_string()
        })?;
    if !response.ok() {
        return Err(format!(
            "the daemon would not take that payload ({})",
            response.status()
        ));
    }
    Ok(())
}

/// Drop a payload the daemon staged for a send that is not going to run.
///
/// Best effort and unawaited by its caller: the send has already failed, and
/// what this prevents is a file nothing will read staying until the account
/// is wiped — staged uploads are deliberately spared by the cache sweep.
pub async fn discard_media(base: &str, key: &str) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let url = format!(
        "{base}/{}{}",
        js_sys::encode_uri_component(key),
        media_token()
    );
    let options = web_sys::RequestInit::new();
    options.set_method("DELETE");
    // Under a deadline like every other request here. Nothing awaits *this*
    // for an answer, but the task holding it is one the staging path can be
    // waiting behind, and a fetch with no signal is one a daemon that has
    // stopped answering never resolves.
    let Ok(abort) = web_sys::AbortController::new() else {
        return;
    };
    options.set_signal(Some(&abort.signal()));
    let Ok(_timeout) = FetchDeadline::arm(&window, &abort, DISCARD_TIMEOUT_MS) else {
        return;
    };
    if let Ok(promise) = window
        .fetch_with_str_and_init(&url, &options)
        .dyn_into::<js_sys::Promise>()
    {
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }
}

/// The same, under a deadline the caller chooses.
///
/// A frame's optional media and a download somebody asked for are not the
/// same errand. The default here is the short one, for the history load whose
/// thumbnails must not stall the stream; a requested attachment is promised a
/// minute, and capping each individual transfer at thirty seconds meant the
/// outer allowance was a fiction — the fetch was aborted, `Frames::apply`
/// found no bytes, and the code waiting reported a failure for something the
/// daemon had already cached.
///
/// # Errors
///
/// The browser refused the request, the bridge did not answer, or the
/// deadline passed.
pub async fn fetch_media_within(
    base: &str,
    key: &str,
    millis: i32,
    most: u64,
) -> Result<Vec<u8>, String> {
    use wasm_bindgen_futures::JsFuture;

    let window = web_sys::window().ok_or("no window to fetch from")?;
    let url = format!(
        "{base}/{}{}",
        js_sys::encode_uri_component(key),
        media_token()
    );

    // Bounded, because the caller resolves a frame's media before it hands
    // the frame on: a bridge that accepts the connection and never answers
    // would otherwise stall that frame for good, with no error to fall back
    // on. An abort turns it into an ordinary failure, which the renderer
    // already draws as an offer to download.
    let abort = web_sys::AbortController::new()
        .map_err(|e| format!("could not arm a fetch timeout: {e:?}"))?;
    let options = web_sys::RequestInit::new();
    options.set_signal(Some(&abort.signal()));
    let _timeout = FetchDeadline::arm(&window, &abort, millis)?;

    /// Aborts the request if this future is dropped before it finishes.
    ///
    /// The caller bounds a whole frame's media as well, and when *that*
    /// deadline wins it simply drops this future — which cancels nothing on
    /// its own, because dropping a `JsFuture` does not cancel the request
    /// behind it, and because [`FetchDeadline`]'s own drop *disarms* the
    /// abort rather than firing it. That is right for the path where the
    /// fetch already answered and wrong for this one, so the two are
    /// separate: one stops the timer, this one stops the request. Without it
    /// every frame that gave up left a browser connection and a daemon slot
    /// held by a request nobody was waiting for any more.
    ///
    /// Unconditional, because aborting a request that has already settled is
    /// a no-op — there is no success path worth disarming for.
    struct AbortOnDrop(web_sys::AbortController);

    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    let _abort_on_drop = AbortOnDrop(abort.clone());

    let response = JsFuture::from(window.fetch_with_str_and_init(&url, &options))
        .await
        .map_err(|e| format!("could not reach the daemon's media bridge: {e:?}"))?
        .dyn_into::<web_sys::Response>()
        .map_err(|_| {
            "the media bridge answered with something that is not a response".to_string()
        })?;
    if !response.ok() {
        return Err(format!(
            "the daemon has no media under {key} ({})",
            response.status()
        ));
    }
    // Before the body is materialized, which is the only place the question
    // can be asked usefully: `array_buffer` allocates the whole payload, so a
    // caller that checks a budget *after* it has already spent what it was
    // trying not to. Dropping out here aborts the request too, through
    // `AbortOnDrop`.
    //
    // Best-effort by nature — a response may carry no `Content-Length`, and
    // then this cannot know until it has read it. That is not a hole worth
    // closing with a streaming reader here: the caller's own running total
    // still stops the *next* fetch, so what an absent length costs is one
    // payload of overshoot rather than an unbounded sequence.
    if let Some(length) = response
        .headers()
        .get("content-length")
        .ok()
        .flatten()
        .and_then(|value| value.trim().parse::<u64>().ok())
        && length > most
    {
        return Err(format!(
            "media {key} is {length} bytes, past the {most} this frame has left"
        ));
    }
    let buffer = JsFuture::from(
        response
            .array_buffer()
            .map_err(|e| format!("unreadable media body: {e:?}"))?,
    )
    .await
    .map_err(|e| format!("unreadable media body: {e:?}"))?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}
