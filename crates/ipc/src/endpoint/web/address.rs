//! What the page was told, and what it may do about it.
//!
//! Where the daemon is (`#daemon=`), whether that is one this page will use,
//! and whether this page may hold an account of its own — three questions
//! answered off the page's own URL and its `<meta>` tags, which is why they
//! are together and why they are neither the socket's nor the media path's.
//! Both of those ask here.
//!
//! The fragment is read before the query, and that is the point rather than a
//! preference: a query string reaches whoever served the page, and the token
//! rides in this URL.

/// Where to look for a daemon, as the page was asked to.
///
/// `?daemon=<url>` first, because the page is static and the daemon is not:
/// one build is served to everybody and each person's daemon is their own.
/// Failing that, the loopback default — which is where a daemon started by
/// hand on the same machine listens.
#[must_use]
pub fn endpoint_url() -> String {
    let default = || {
        format!(
            "ws://127.0.0.1:{}{}",
            crate::DEFAULT_WEB_PORT,
            crate::WEB_SOCKET_PATH
        )
    };
    match named_daemon() {
        NamedDaemon::Named(asked) => asked,
        // A rejected one falls back here on purpose: this function answers
        // "where would a daemon be", and the caller that must not proceed on a
        // rejection is the one that matches on [`named_daemon`] itself.
        NamedDaemon::Nobody | NamedDaemon::Rejected(_) => default(),
    }
}

/// What this page was told to attach to.
///
/// Three answers, and the third is why this is not an `Option`. "Nobody named
/// one" is a page that runs its own session; "named one we will not use" is a
/// configuration error, and collapsing it into the first silently opens a
/// *different* session — against this origin's own store — for somebody whose
/// only mistake was a typo in a URL, or whose daemon was refused for exactly
/// the reason the check exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamedDaemon {
    /// No `daemon` parameter at all.
    Nobody,
    /// One named, and usable.
    Named(String),
    /// One named, and refused. The string is for a person, and carries no
    /// token.
    Rejected(String),
}

/// The daemon this page was pointed at, if it was pointed at one.
#[must_use]
pub fn named_daemon() -> NamedDaemon {
    let asked = match read_parameter("daemon") {
        Parameter::Present(asked) => asked,
        Parameter::Absent => return NamedDaemon::Nobody,
        // Named, and not readable — a truncated `%` in a pasted URL is the
        // usual way. Refused rather than ignored: ignoring it opens a session
        // against this origin's own store for somebody who asked for a
        // daemon, which is the substitution `Rejected` exists to prevent.
        Parameter::Unreadable => {
            log::error!("ignoring #daemon=: the value is not decodable");
            return NamedDaemon::Rejected(
                "The #daemon in this page's address could not be read — a percent escape in it \
                 is incomplete. Correct it, or remove it to let this page run its own session."
                    .to_string(),
            );
        }
    };
    // A query parameter is whatever put the user on this page, which may be a
    // link somebody sent them. The daemon it names is handed the message
    // history and can be told to send, so an unchecked one turns a link into
    // a way to point the window at somebody else's server.
    match usable_endpoint(&asked) {
        Ok(()) => NamedDaemon::Named(asked),
        Err(why) => {
            // Redacted, here and in what is shown. A rejected URL is the
            // *likeliest* one to be pasted into an issue — it is the one that
            // did not work — and it carries the same token the accepted one
            // does.
            let named = without_secrets(&asked);
            log::error!("ignoring #daemon={named}: {why}");
            NamedDaemon::Rejected(format!(
                "This page was pointed at {named}, which it will not use: {why}. \
                 Correct the #daemon in the address, or remove it to let this \
                 page run its own session."
            ))
        }
    }
}

/// # Known gap: the daemon is not authenticated to the page
///
/// The token proves the *page* to the daemon. Nothing proves the daemon to
/// the page, and on a loopback TCP port that asymmetry has teeth: another
/// account on the machine can bind the predictable port first, and a
/// bookmarked URL opened while the real daemon is down hands that process the
/// token in its handshake. It can then release the port, wait, and use the
/// token against the real daemon — `Origin` is a string it also controls.
///
/// The native endpoint has no such gap: a Unix socket has a peer uid, and the
/// client checks who answered. A browser cannot ask that of a TCP port.
///
/// Closing it means mutual authentication, and the shapes trade against each
/// other rather than one being obviously right:
///
/// - **Server-first challenge.** Connect carrying nothing, send a nonce, and
///   let the daemon prove it holds the token before the page offers its own
///   proof. Nothing is ever disclosed to an impostor. The cost is that the
///   upgrade becomes unauthenticated, so the endpoint stops being able to
///   answer `404` to strangers — the concealment described above is spent to
///   buy this.
/// - **Proof in the query.** Send `HMAC(token, nonce)` instead of the token.
///   Keeps the `404`, but a proof replayed with its own nonce is as good as
///   the token unless the daemon remembers nonces, which is state and a
///   clock.
///
/// Both are a wire-protocol change on two ends plus the media path, which
/// authenticates per request. Until one is chosen, `--web` carries this: it
/// is off by default, and the threat is a hostile account on the same
/// machine.

/// A URL fit to be written down.
///
/// The query is where the token lives, so it is what comes off: everything
/// that identifies *which* daemon survives, and the credential that admits
/// you to it does not. Not a parser — a token is only ever in the query, and
/// splitting there cannot accidentally keep one.
#[must_use]
pub fn without_secrets(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

/// Whether a page may attach to this URL without being asked again.
///
/// Two rules. It has to be a WebSocket URL, because anything else is a
/// mistake or an attempt at something. And it has to name either this machine
/// or the origin the page was itself served from — a daemon somewhere else
/// entirely is a decision, not a default, and a link is not how it should be
/// made.
///
/// Parsed with the browser's own URL parser rather than by splitting on
/// characters, because *the browser* is what will resolve it and only its
/// answer is the one that matters. Splitting is how
/// `wss://127.0.0.1:9527@evil.example/ws` gets through a host check: the part
/// before the `@` is userinfo, so a reader looking for a colon finds
/// `127.0.0.1` while the socket opens to `evil.example`.
///
/// # Errors
///
/// The reason, for the log: this is a silent fallback to the loopback default
/// rather than a failure to start, because a page that refuses to load is
/// worse than one that attaches where it was going to anyway.
fn usable_endpoint(url: &str) -> Result<(), String> {
    let parsed = web_sys::Url::new(url).map_err(|_| "not a URL".to_string())?;
    if !matches!(parsed.protocol().as_str(), "ws:" | "wss:") {
        return Err(format!("{} is not a WebSocket scheme", parsed.protocol()));
    }
    // `hostname`, not `host`: no port, no userinfo, and already lowercased
    // and unwrapped from the brackets an IPv6 literal carries.
    let host = parsed.hostname();
    if crate::endpoint::is_loopback_host(&host) {
        return Ok(());
    }
    // The page's own origin: a deployment that serves the bridge beside
    // itself is naming where it already came from.
    //
    // The *whole* origin, not the hostname. A host is not an origin — a
    // different port is a different origin, and on a shared or nonstandard
    // host it is very likely a different owner. Comparing hostnames alone let
    // `#daemon=wss://this-host:8443/ws` pass as "where this page came from",
    // which handed the window, and everything typed into it, to whatever
    // answers on that port.
    //
    // `host()` rather than `hostname()` because it carries the port, and it
    // omits the default one on both sides — so `wss://example.com/ws` still
    // matches a page served from `https://example.com`.
    let Some(location) = web_sys::window().map(|window| window.location()) else {
        return Err(format!(
            "{host} is not this machine, and there is no page to compare it to"
        ));
    };
    let (page_scheme, page_host) = (
        location.protocol().unwrap_or_default(),
        location.host().unwrap_or_default(),
    );
    // A page's scheme decides which socket scheme is the same origin: an
    // `https:` page reaching `ws:` is a downgrade, and an `http:` page
    // reaching `wss:` is naming somewhere it did not come from.
    let expected = match page_scheme.as_str() {
        "https:" => "wss:",
        "http:" => "ws:",
        _ => "",
    };
    if !page_host.is_empty()
        && parsed.protocol() == expected
        && parsed.host().eq_ignore_ascii_case(&page_host)
    {
        return Ok(());
    }
    Err(format!(
        "{}//{} is neither this machine nor where this page came from",
        parsed.protocol(),
        parsed.host()
    ))
}

/// Whether this page is a preview rather than the deployment.
///
/// Declared by the page itself — a `<meta name="oxidezap-build" content="preview">`
/// the publisher puts there — and not guessed from the path, because the
/// consequence is too sharp for a guess. A preview shares its origin with the
/// deployment: same scheme, same host, same port, a different directory. That
/// was harmless while the page held nothing, and it is not now. A page that
/// runs its own session keeps the account in origin-scoped storage, and an
/// origin is not a directory — so unmerged code served under `/pr/<n>/` can
/// read the deployment's database, credentials and all, with no token
/// anywhere in the way.
///
/// Absent means not a preview, which is the safe direction: a deployment that
/// somehow lost the tag runs its own session as it should, and a preview that
/// somehow lost it is a preview nobody should have been pointing at an
/// account anyway.
///
/// The refusal it drives is a default rather than a wall — see
/// [`session_allowed_here`] — and it is worth being clear about what kind of
/// thing it is. It stops somebody wandering into a preview and pairing an
/// account there beside the deployment's. It is **not** a boundary: a preview
/// is built from its own branch's source, so that branch is free to delete
/// this check, and origin-scoped storage is readable by anything on the
/// origin regardless. What bounds that is who may publish a preview at all —
/// same-repository branches, which already require push access. See the
/// header of `.github/workflows/pages.yml`.
#[must_use]
pub fn is_preview() -> bool {
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return false;
    };
    let Ok(Some(meta)) = document.query_selector("meta[name='oxidezap-build']") else {
        return false;
    };
    meta.get_attribute("content").as_deref() == Some("preview")
}

/// Whether this page may hold an account of its own.
///
/// Everything but a preview may. A preview may too, and only when somebody
/// asks for it in the URL — `#preview-session` — because the person testing
/// unmerged code on a preview is the one person who *wants* it to hold an
/// account, and refusing them outright makes the preview useless for the
/// thing it exists to preview.
///
/// The opt-in is what makes the default honest rather than absolute. Nobody
/// reaches this by following a link: the account a preview would share the
/// origin with is the deployment's, and someone who types the flag has said
/// they know whose database is one directory over. What it does not do is
/// make the two safe from each other — an origin is not a directory, and no
/// flag changes that. It moves the decision to a person.
#[must_use]
pub fn session_allowed_here() -> bool {
    !is_preview() || flag_present("preview-session")
}

/// A bare word in the fragment or the query, with no value after it.
///
/// [`find_parameter`] deliberately skips a pair it cannot split, because a
/// valueless `daemon` is a typo rather than a request. A flag is the opposite:
/// the word *is* the request, and `#preview-session=1` would be asking someone
/// to type a value that means nothing.
fn flag_present(name: &str) -> bool {
    let Some(location) = web_sys::window().map(|window| window.location()) else {
        return false;
    };
    let names = |text: String| {
        text.trim_start_matches(['#', '?'])
            .split('&')
            .any(|pair| pair == name)
    };
    location.hash().is_ok_and(names) || location.search().is_ok_and(names)
}

/// One query parameter off the page's own URL, or why there is none.
///
/// Three answers rather than two, for the same reason [`NamedDaemon`] has
/// three: "nobody wrote one" and "somebody wrote one this cannot read" lead
/// to opposite places. A truncated `%` in a pasted URL is the ordinary way to
/// arrive at the second, and collapsing it into the first started a session
/// against the browser's own store for somebody who had asked for a daemon.
enum Parameter {
    /// Not in the fragment or the query.
    Absent,
    /// There, and not decodable — a malformed percent escape.
    Unreadable,
    /// There, and decoded.
    Present(String),
}

/// One query parameter off the page's own URL, as one of the three answers
/// above rather than as an `Option` that would lose the third.
fn read_parameter(name: &str) -> Parameter {
    let Some(window) = web_sys::window() else {
        return Parameter::Absent;
    };
    let location = window.location();

    // The fragment first, and it is where the answer is meant to be.
    //
    // A page's query string is sent to whoever served the page — it is in the
    // request line — so a token carried there reaches the static host's logs
    // before a single line of this runs. The fragment is never sent: browsers
    // strip it from the request, which is exactly why the implicit OAuth flow
    // used it for the same purpose.
    if let Ok(hash) = location.hash() {
        match find_parameter(hash.trim_start_matches('#'), name) {
            found @ (Parameter::Present(_) | Parameter::Unreadable) => return found,
            Parameter::Absent => {}
        }
    }

    // The query still answers, because refusing would not un-send it. What it
    // does is say so: the URL is already in somebody's logs, and the only
    // repair is a new token and a bookmark that uses `#`.
    let Ok(search) = location.search() else {
        return Parameter::Absent;
    };
    let found = find_parameter(search.trim_start_matches('?'), name);
    if matches!(found, Parameter::Present(_)) {
        log::warn!(
            "?{name}= was read from the query string, which the page's host has already been \
             sent. Put it after a `#` instead — and if it carried a token, draw a new one."
        );
    }
    found
}

/// One `key=value` out of an `&`-separated list.
fn find_parameter(pairs: &str, name: &str) -> Parameter {
    for pair in pairs.split('&') {
        // `continue`, not `?`. Returning from the whole function on the first
        // parameter without a value made `?debug&daemon=…` resolve to nothing
        // and fall silently back to the loopback default.
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key == name {
            // Undecodable is an answer, not the absence of one: the name is
            // there and this is the value somebody meant.
            return match decode_component(value) {
                Some(decoded) => Parameter::Present(decoded),
                None => Parameter::Unreadable,
            };
        }
    }
    Parameter::Absent
}

/// Percent-decoding, through the browser's own decoder.
///
/// `decodeURIComponent` is right there and is the exact inverse of whatever
/// produced the URL; a hand-rolled one would be a second answer to a question
/// the platform has already answered.
fn decode_component(value: &str) -> Option<String> {
    js_sys::decode_uri_component(value)
        .ok()
        .and_then(|decoded| decoded.as_string())
}
