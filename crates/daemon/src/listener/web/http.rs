//! The little HTTP this port speaks before it is a WebSocket.
//!
//! A WebSocket upgrade *is* an HTTP request, and the same port answers the
//! media sideband beside it — so something has to read a request head, decide
//! whether the caller may be here at all, and write a response. That is this
//! module, and it is deliberately the smallest thing that does it: no router,
//! no server, just the handful of headers this endpoint reads and the three
//! answers it writes.
//!
//! Admission lives here because it is asked of a *request* rather than of a
//! route: [`Request::origin_allowed`] narrows who may probe and
//! [`Request::token_matches`] is the check the whole endpoint stands on. Both
//! are asked once, in [`super::serve`], before anything — the socket, the
//! media, even the preflight — is served.

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;

use super::Config;

/// How long a connection has to send its request line and headers.
///
/// Small: this is a local socket and the whole head is one packet. A peer
/// that connects and says nothing otherwise holds a task for as long as it
/// likes, which is the same reason the IPC handshake is bounded.
pub(super) const HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The most a request head may be before it is refused.
const MAX_HEAD: usize = 16 * 1024;

/// Read up to the blank line that ends a request head.
pub(super) async fn read_head(stream: &mut BufReader<TcpStream>) -> Result<String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if head.len() > MAX_HEAD {
            anyhow::bail!("request head over {MAX_HEAD} bytes");
        }
        let read = stream.read(&mut byte).await?;
        if read == 0 {
            anyhow::bail!("the connection closed before the request ended");
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(head).context("the request head is not text")
}

/// As much of a request as this bridge needs.
pub(super) struct Request {
    pub(super) method: String,
    pub(super) path: String,
    /// Everything after the `?`, which is where the token rides.
    query: String,
    pub(super) origin: Option<String>,
    pub(super) websocket_key: Option<String>,
    /// Whether this preflight is asking to reach a private address, which is
    /// what a page served from a public origin has to ask before it may talk
    /// to loopback at all.
    pub(super) wants_private_network: bool,
    /// The declared body length, which only a staging upload has.
    ///
    /// Read rather than trusted: it decides how much is read, so
    /// [`crate::media::http::receive`] refuses one past its ceiling before a byte arrives
    /// rather than discovering the size by accepting it.
    pub(super) content_length: Option<u64>,
}

impl Request {
    pub(super) fn parse(head: &str) -> Option<Self> {
        let mut lines = head.split("\r\n");
        let mut start = lines.next()?.split(' ');
        let method = start.next()?.to_string();
        // The query is not part of any route, but it is where the token is,
        // so it is kept rather than discarded.
        let target = start.next()?;
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let path = path.to_string();
        let query = query.to_string();

        let mut origin = None;
        let mut websocket_key = None;
        let mut wants_private_network = false;
        let mut content_length = None;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim().to_string();
            // Header names are case-insensitive, and browsers do not agree
            // on which case they send.
            match name.to_ascii_lowercase().as_str() {
                "origin" => origin = Some(value),
                "sec-websocket-key" => websocket_key = Some(value),
                "access-control-request-private-network" => {
                    wants_private_network = value.eq_ignore_ascii_case("true");
                }
                "content-length" => content_length = value.trim().parse::<u64>().ok(),
                _ => {}
            }
        }

        Some(Self {
            method,
            path,
            query,
            origin,
            websocket_key,
            wants_private_network,
            content_length,
        })
    }

    /// Whether this origin is one the user said to serve.
    ///
    /// Not on its own an admission check — [`Self::token_matches`] is — but
    /// still worth keeping: a page the user never named has no business
    /// reaching this endpoint even holding a token, and refusing it here is
    /// what stops one from being probed for.
    pub(super) fn origin_allowed(&self, config: &Config) -> bool {
        let Some(origin) = &self.origin else {
            // A missing `Origin` says nothing about who is asking: an
            // `<img src>`, a `<script src>` and a form GET are all browser
            // requests that carry none. So this branch is not "not a
            // browser", it is "nothing to check" — which is why the token
            // is what admits it, and why this is served only on a loopback
            // bind, where `run` already refuses to be anywhere else.
            return config.addr.ip().is_loopback();
        };
        if is_loopback_origin(origin) {
            return true;
        }
        config
            .allowed_origins
            .iter()
            .any(|allowed| allowed == origin)
    }

    /// Whether this request carries the daemon's own token.
    ///
    /// The admission check. A Unix socket is inside a `0700` directory and
    /// answers a peer's uid, so reaching it *is* proof of being this user; a
    /// loopback TCP port is none of those things — every account on the
    /// machine can connect to it, and every one of them can write
    /// `Origin: http://localhost`, which is a string. The token is the piece
    /// that carries the socket's guarantee across: it lives in that same
    /// per-user directory, so knowing it is proof of being able to read it.
    ///
    /// Compared without an early return, so the number of leading bytes that
    /// matched is not something a caller can time.
    pub(super) fn token_matches(&self, config: &Config) -> bool {
        let Some(offered) = parameter(&self.query, "token") else {
            return false;
        };
        let expected = config.token.as_bytes();
        let offered = offered.as_bytes();
        if offered.len() != expected.len() {
            return false;
        }
        let mut difference = 0u8;
        for (a, b) in expected.iter().zip(offered) {
            difference |= a ^ b;
        }
        difference == 0
    }
}

/// One parameter out of a query string, percent-decoded.
fn parameter(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| percent_decode(value))
    })
}

/// Whether an origin is this machine talking to itself.
///
/// The developer's own `trunk serve`, which is the case that would otherwise
/// need configuring on every checkout. Matched on the host rather than by
/// prefix, so `https://localhost.example.com` is not mistaken for one.
///
/// The address half goes through `IpAddr::is_loopback` rather than a literal,
/// which is what the client end and the bind check already do: the whole of
/// `127.0.0.0/8` is this machine, a page served from `http://127.0.0.2:8080`
/// is as much the developer's own as one from `127.0.0.1`, and three places
/// answering "is this loopback" differently is how a page gets refused by the
/// daemon it is allowed to bind.
fn is_loopback_origin(origin: &str) -> bool {
    let without_scheme = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .unwrap_or(origin);
    let authority = without_scheme.split('/').next().unwrap_or(without_scheme);
    // A bracketed IPv6 literal is full of colons, so the port cannot be split
    // off from the right without cutting the address itself: `http://[::1]`
    // has no port at all — a browser omits the default — and splitting it
    // produced the host `[:`, which refused the page its own daemon was
    // serving. The bracket is where the address ends.
    let host = if let Some(rest) = authority.strip_prefix('[') {
        match rest.split_once(']') {
            Some((inside, _port)) => inside,
            // No closing bracket: not an authority we can read, so not one we
            // will call loopback.
            None => return false,
        }
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host)
    };
    if host == "localhost" {
        return true;
    }
    // A bracketed literal arrives here unbracketed, which is what
    // `Ipv6Addr` parses; anything that is not an address at all — a real
    // hostname — is not loopback, whatever it resolves to today.
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

/// The little decoding a media key can need.
///
/// The client encodes the key with `encodeURIComponent`; a key is already
/// restricted to characters that survive that untouched, so this exists for
/// the one that does not survive being *sent* — and anything malformed falls
/// through to `media_path`, which refuses it.
pub(crate) fn percent_decode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut bytes = raw.bytes();
    while let Some(byte) = bytes.next() {
        if byte != b'%' {
            out.push(byte as char);
            continue;
        }
        let high = bytes.next().and_then(|b| (b as char).to_digit(16));
        let low = bytes.next().and_then(|b| (b as char).to_digit(16));
        match (high, low) {
            (Some(high), Some(low)) => out.push(((high * 16 + low) as u8) as char),
            // Malformed: keep it as written and let `media_path` refuse it.
            _ => out.push('%'),
        }
    }
    out
}
/// Answer a preflight, including the one a page needs to reach loopback at all.
///
/// A hosted page is a *public* origin asking a *private* address, which Chrome
/// gates behind Private Network Access: the preflight carries
/// `Access-Control-Request-Private-Network`, and the fetch only follows if the
/// answer carries `Access-Control-Allow-Private-Network: true`. Without it the
/// socket attaches and every photo fails — the WebSocket is not subject to
/// this and `fetch` is, so the failure looks like a working connection with no
/// media in it.
///
/// Only when asked, and only after the token check above: this says "yes, a
/// public page may reach this private address", which is exactly the sentence
/// that should require the credential. Answering it unconditionally would
/// advertise the opt-in to callers who have not authenticated.
pub(super) async fn preflight(
    stream: &mut TcpStream,
    origin: Option<&str>,
    private_network: bool,
) -> Result<()> {
    stream
        .write_all(preflight_head(origin, private_network).as_bytes())
        .await?;
    stream.flush().await?;
    Ok(())
}

/// The preflight's headers, so what a browser is told can be asserted.
fn preflight_head(origin: Option<&str>, private_network: bool) -> String {
    let mut head = String::from(
        "HTTP/1.1 204 No Content\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n",
    );
    if let Some(origin) = origin {
        // Without a lifetime a browser picks its own, and Chrome's is five
        // seconds — which is one preflight per photo, and a preflight is a
        // whole connection and round trip before the one that fetches. A
        // history load naming a hundred photos paid for two hundred.
        head.push_str("Access-Control-Max-Age: 600\r\n");
        head.push_str(&format!(
            "Access-Control-Allow-Origin: {origin}\r\n\
             Access-Control-Allow-Methods: GET, PUT, DELETE, OPTIONS\r\n\
             Access-Control-Allow-Headers: Content-Type\r\n\
             Vary: Origin\r\n"
        ));
    }
    if private_network {
        head.push_str("Access-Control-Allow-Private-Network: true\r\n");
    }
    head.push_str("\r\n");
    head
}

/// One HTTP response, headers and all.
pub(crate) async fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    origin: Option<&str>,
    body: &[u8],
) -> Result<()> {
    // Every status any caller picks, including the four the media sideband
    // picks in `crate::media::http` — a refusal that reads
    // `413 Not Found` is a wire line that contradicts itself, and the two
    // halves no longer sit in one file where the mismatch would be obvious.
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        411 => "Length Required",
        413 => "Content Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Not Found",
    };
    // `no-store`, because the media directory is meant to be the whole of
    // what "clear cached media" and "forget this account" have to delete.
    // A browser allowed to keep a copy of a decrypted photo in its own HTTP
    // cache puts it somewhere neither `Wipe::Cache` nor `Wipe::Everything`
    // can reach, on a disk the daemon does not manage — so the deletion
    // boundary would quietly stop being one.
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n",
        body.len()
    );
    // Echoed rather than `*`: the origin reaching here has already been
    // checked against what the user allowed, and naming it keeps the answer
    // scoped to the page that asked.
    if let Some(origin) = origin {
        head.push_str(&format!(
            "Access-Control-Allow-Origin: {origin}\r\n\
             Access-Control-Allow-Methods: GET, PUT, DELETE, OPTIONS\r\n\
             Vary: Origin\r\n"
        ));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A token the tests can also write into a request.
    pub(crate) const TEST_TOKEN: &str = "0123456789abcdef";

    fn config(origins: &[&str]) -> Config {
        Config {
            addr: "127.0.0.1:0".parse().expect("a literal address"),
            allowed_origins: origins.iter().map(|o| (*o).to_string()).collect(),
            token: TEST_TOKEN.to_string(),
        }
    }

    fn request(origin: Option<&str>) -> Request {
        asking(&format!("token={TEST_TOKEN}"), origin)
    }

    fn asking(query: &str, origin: Option<&str>) -> Request {
        Request {
            method: "GET".into(),
            path: oxidezap_ipc::WEB_SOCKET_PATH.into(),
            query: query.to_string(),
            origin: origin.map(str::to_string),
            websocket_key: None,
            wants_private_network: false,
            content_length: None,
        }
    }

    /// The check this endpoint exists behind. A WebSocket is not subject to
    /// the same-origin policy, so any page in the user's browser can reach
    /// the port; only this says which one may.
    #[test]
    fn an_unnamed_origin_is_refused() {
        let config = config(&[]);
        assert!(!request(Some("https://evil.example")).origin_allowed(&config));
        assert!(!request(Some("https://oxidezap.github.io")).origin_allowed(&config));
    }

    #[test]
    fn a_named_origin_is_served() {
        let config = config(&["https://oxidezap.github.io"]);
        assert!(request(Some("https://oxidezap.github.io")).origin_allowed(&config));
        assert!(!request(Some("https://evil.example")).origin_allowed(&config));
    }

    /// A development build served from the machine itself, which would
    /// otherwise need configuring on every checkout.
    #[test]
    fn the_developers_own_page_is_served_without_being_named() {
        let config = config(&[]);
        for origin in [
            "http://localhost:8080",
            "http://127.0.0.1:8080",
            "http://[::1]:8080",
        ] {
            assert!(
                request(Some(origin)).origin_allowed(&config),
                "{origin} was refused"
            );
        }
    }

    /// The suffix trick: a host that merely *ends* in something loopback-ish
    /// is somebody else's machine.
    #[test]
    fn a_host_that_only_looks_local_is_not() {
        let config = config(&[]);
        for origin in [
            "https://localhost.evil.example",
            "https://127.0.0.1.evil.example",
            "https://notlocalhost",
        ] {
            assert!(
                !request(Some(origin)).origin_allowed(&config),
                "{origin} was served"
            );
        }
    }

    /// A preflight with no lifetime on it is one the browser repeats, and
    /// Chrome repeats it after five seconds — so a history load naming a
    /// hundred photos spends a hundred extra round trips before the fetches
    /// that carry the pictures, and the ones that miss the frame's budget are
    /// drawn as media that is not there.
    #[test]
    fn a_preflight_says_how_long_it_is_good_for() {
        let head = preflight_head(Some("https://oxidezap.github.io"), true);
        assert!(
            head.contains("Access-Control-Max-Age: 600\r\n"),
            "the browser was told nothing, so it asks again per photo: {head}"
        );
    }

    /// A client that sends no `Origin` carries nothing to check, so it is
    /// served only where the reach is the machine itself — and still only
    /// with the token.
    #[test]
    fn a_client_with_no_origin_is_served_only_on_loopback() {
        assert!(request(None).origin_allowed(&config(&[])));
        assert!(request(None).origin_allowed(&config(&["https://oxidezap.github.io"])));
    }

    #[test]
    fn a_request_line_is_read_without_its_query() {
        let head = "GET /ws?daemon=x HTTP/1.1\r\n\
                    Host: 127.0.0.1:9527\r\n\
                    Origin: http://localhost:8080\r\n\
                    Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n";
        let request = Request::parse(head).expect("a well-formed head");
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/ws");
        assert_eq!(request.origin.as_deref(), Some("http://localhost:8080"));
        assert_eq!(
            request.websocket_key.as_deref(),
            Some("dGhlIHNhbXBsZSBub25jZQ==")
        );
    }

    /// Header names are case-insensitive and browsers do not agree.
    #[test]
    fn header_names_are_matched_whatever_their_case() {
        let head = "GET /ws HTTP/1.1\r\nORIGIN: http://localhost:1\r\nsec-websocket-key: k\r\n\r\n";
        let request = Request::parse(head).expect("a well-formed head");
        assert_eq!(request.origin.as_deref(), Some("http://localhost:1"));
        assert_eq!(request.websocket_key.as_deref(), Some("k"));
    }

    /// The check the whole endpoint stands on. Reaching a loopback port is
    /// not proof of anything — every account on the machine can — so without
    /// this another local user has the message history and the ability to
    /// send.
    #[test]
    fn a_request_without_the_token_is_refused() {
        for query in [
            "",
            "token=",
            "token=wrong",
            // A prefix of the real one, and one that extends it: neither is
            // it.
            "token=0123456789abcde",
            "token=0123456789abcdef0",
            "tokens=0123456789abcdef",
            "daemon=ws://127.0.0.1:9527/ws",
        ] {
            assert!(
                !asking(query, None).token_matches(&config(&[])),
                "{query:?} was admitted"
            );
        }
    }

    /// And the one that carries it is let through, wherever in the query it
    /// sits.
    #[test]
    fn a_request_with_the_token_is_admitted() {
        for query in [
            "token=0123456789abcdef",
            "daemon=x&token=0123456789abcdef",
            "token=0123456789abcdef&cache=1",
        ] {
            assert!(
                asking(query, None).token_matches(&config(&[])),
                "{query:?} was refused"
            );
        }
    }

    /// Every shape a browser writes for this machine talking to itself,
    /// including the two an IPv6 literal takes: a browser omits the default
    /// port, so `http://[::1]` arrives with no port to strip and brackets
    /// that a right-hand split walks straight into.
    #[test]
    fn a_loopback_origin_is_recognised_in_every_shape_a_browser_writes_it() {
        for origin in [
            "http://localhost",
            "http://localhost:8080",
            "https://127.0.0.1",
            "http://127.0.0.1:8080",
            "http://[::1]",
            "http://[::1]:8080",
            // The rest of `127.0.0.0/8`, which the bind check and the client
            // end already call loopback. A literal here refused a page the
            // daemon would have served.
            "http://127.0.0.2:8080",
            "http://127.1.2.3",
        ] {
            assert!(is_loopback_origin(origin), "{origin} was not recognised");
        }
        for elsewhere in [
            "https://localhost.example.com",
            "https://example.com",
            "http://127.0.0.1.example.com",
            // Unreadable rather than loopback: an unclosed bracket is not an
            // authority, and guessing at one is how `[::1].evil.com` gets in.
            "http://[::1",
        ] {
            assert!(!is_loopback_origin(elsewhere), "{elsewhere} was admitted");
        }
    }

    /// The token is pasted into a URL, so it arrives percent-encoded when it
    /// has to be. It is hex today and would not need decoding; relying on
    /// that is what makes a later change to the alphabet a silent lockout.
    #[test]
    fn a_percent_encoded_token_is_decoded_before_it_is_compared() {
        let config = Config {
            addr: "127.0.0.1:0".parse().expect("a literal address"),
            allowed_origins: Vec::new(),
            token: "a b".to_string(),
        };
        assert!(asking("token=a%20b", None).token_matches(&config));
    }
}
