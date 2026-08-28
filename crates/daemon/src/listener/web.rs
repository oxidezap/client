//! The daemon's third endpoint: the one a browser can reach.
//!
//! A Unix socket and a named pipe are both things only a process on this
//! machine can open, which is exactly why they were chosen — the endpoint
//! carries control of a WhatsApp session. A page can open neither. So a front
//! end that is a tab gets a TCP port on loopback speaking the same
//! newline-delimited JSON over a WebSocket, plus the media sideband that
//! protocol depends on, served over HTTP beside it.
//!
//! # Nothing about the protocol is repeated here
//!
//! [`crate::server::serve_client`] is generic over `AsyncRead + AsyncWrite`,
//! and a WebSocket is neither: it carries whole messages rather than bytes.
//! Rather than teach the server a second shape, this bridges the two with a
//! `tokio::io::duplex` pair — the server reads and writes lines into one end
//! and this module moves them across as text frames. Handshake, version
//! check, snapshot, state versioning and every request are the same code the
//! desktop connection runs.
//!
//! # Why it is off by default, and why origins are checked
//!
//! A WebSocket is not subject to the same-origin policy: any page in the
//! user's browser can open one to `ws://127.0.0.1`, and this endpoint would
//! hand it the message history and let it send. A Unix socket has file
//! permissions and a peer uid to lean on; a loopback port has neither, so the
//! `Origin` header is the only thing that distinguishes the page the user
//! opened from a page that merely knows the port number.
//!
//! It is therefore opt-in (`--web`), bound to loopback unless told otherwise,
//! and refuses any browser origin that was not named (`--web-allow`) —
//! excepting `localhost` and `127.0.0.1`, which are the developer's own
//! `trunk serve`. A request with no `Origin` at all is not a browser and is
//! left to the same rule, because a page cannot suppress the header and a
//! hand-written client that omits it should not be privileged over one that
//! sends it.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine as _;
use futures_util::{SinkExt as _, StreamExt as _};
use oxidezap_ipc::{WEB_MEDIA_PATH, WEB_SOCKET_PATH};
use sha1::{Digest as _, Sha1};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::Role;

use crate::server::{self, ClientSlots};
use crate::session_bridge::Commands;
use crate::state::StateHub;

/// What the bridge was asked to be.
#[derive(Clone, Debug)]
pub struct Config {
    /// Where to listen. Loopback unless the user said otherwise.
    pub addr: SocketAddr,
    /// Browser origins allowed to attach, beyond the loopback ones.
    pub allowed_origins: Vec<String>,
}

/// The magic string RFC 6455 mixes into the accept key. Not a secret: it
/// exists so a cache or a proxy cannot be tricked into completing a
/// handshake it did not understand.
const WS_MAGIC: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// How long a connection has to send its request line and headers.
///
/// Small: this is a local socket and the whole head is one packet. A peer
/// that connects and says nothing otherwise holds a task for as long as it
/// likes, which is the same reason the IPC handshake is bounded.
const HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The most a request head may be before it is refused.
const MAX_HEAD: usize = 16 * 1024;

/// Serve until the future is dropped.
///
/// # Errors
///
/// The port could not be bound. Per-connection failures are logged and
/// dropped, exactly as the IPC listener treats them.
pub async fn run(
    config: Config,
    hub: Arc<StateHub>,
    commands: Commands,
    slots: ClientSlots,
) -> Result<()> {
    let listener = TcpListener::bind(config.addr)
        .await
        .with_context(|| format!("binding the web bridge to {}", config.addr))?;
    log::info!(
        "web bridge listening on http://{}{WEB_SOCKET_PATH} (origins: {})",
        config.addr,
        if config.allowed_origins.is_empty() {
            "loopback only".to_string()
        } else {
            config.allowed_origins.join(", ")
        }
    );
    if !config.addr.ip().is_loopback() {
        // Said once, loudly. Off the loopback there is nothing between this
        // endpoint and the network but the `Origin` header, which only a
        // browser is obliged to send truthfully — see the module
        // documentation. Anything without one is refused there.
        log::warn!(
            "the web bridge is bound to {}, which is not loopback: anything that can \
             reach that address can reach this session, and only a browser's own \
             origin check stands in the way",
            config.addr
        );
    }

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                log::warn!("skipping a web connection we could not accept: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        let config = config.clone();
        let hub = Arc::clone(&hub);
        let commands = commands.clone();
        let slots = Arc::clone(&slots);
        tokio::spawn(async move {
            if let Err(e) = serve(stream, &config, hub, commands, slots).await {
                log::debug!("web client {peer} disconnected: {e}");
            }
        });
    }
}

/// One connection: a WebSocket upgrade, a media request, or a refusal.
async fn serve(
    stream: TcpStream,
    config: &Config,
    hub: Arc<StateHub>,
    commands: Commands,
    slots: ClientSlots,
) -> Result<()> {
    let mut stream = BufReader::new(stream);
    let head = match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut stream)).await {
        Ok(head) => head?,
        Err(_) => anyhow::bail!("no request within {HEAD_TIMEOUT:?}"),
    };
    let request = Request::parse(&head).context("unparsable request")?;

    // The one thing checked before anything else is served: an origin that
    // was not named gets nothing, not even a 404 that would confirm the port.
    if !request.origin_allowed(config) {
        log::warn!(
            "refusing a web client from origin {:?}",
            request.origin.as_deref().unwrap_or("(none)")
        );
        respond(
            stream.get_mut(),
            403,
            "text/plain",
            None,
            b"this daemon was not told to accept that origin",
        )
        .await?;
        return Ok(());
    }
    let origin = request.origin.clone();

    // A browser asks before it fetches media from another origin.
    if request.method == "OPTIONS" {
        return respond(stream.get_mut(), 204, "text/plain", origin.as_deref(), b"").await;
    }

    if request.path == WEB_SOCKET_PATH {
        let Some(key) = request.websocket_key.clone() else {
            return respond(
                stream.get_mut(),
                400,
                "text/plain",
                origin.as_deref(),
                b"this path is a WebSocket endpoint",
            )
            .await;
        };
        // The cap is claimed here and not at accept: it counts *front ends*,
        // and this port also answers media requests. Taken at accept, a page
        // fetching its photos would spend the same slots as the connections
        // it is fetching them for — thirty-two attached windows would refuse
        // every one of their own media requests, and one in flight could take
        // the last slot from a real client.
        let Ok(slot) = slots.try_acquire_owned() else {
            server::reject(stream.into_inner()).await;
            return Ok(());
        };
        let served = attach(stream, &key, hub, commands).await;
        drop(slot);
        return served;
    }

    if let Some(key) = request.path.strip_prefix(&format!("{WEB_MEDIA_PATH}/")) {
        return serve_media(stream.get_mut(), key, origin.as_deref()).await;
    }

    respond(
        stream.get_mut(),
        404,
        "text/plain",
        origin.as_deref(),
        b"nothing here",
    )
    .await
}

/// Complete the upgrade, then let the ordinary server do the talking.
async fn attach(
    mut stream: BufReader<TcpStream>,
    key: &str,
    hub: Arc<StateHub>,
    commands: Commands,
) -> Result<()> {
    let accept = {
        let mut hasher = Sha1::new();
        hasher.update(key.as_bytes());
        hasher.update(WS_MAGIC.as_bytes());
        base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
    };
    stream
        .get_mut()
        .write_all(
            format!(
                "HTTP/1.1 101 Switching Protocols\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Accept: {accept}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .context("writing the upgrade response")?;

    // `from_raw_socket` rather than `accept_async`: the handshake was read
    // and answered above, because this port also serves media and the request
    // had to be routed before it could be upgraded.
    let socket = WebSocketStream::from_raw_socket(stream.into_inner(), Role::Server, None).await;
    let (mut outbound, mut inbound) = socket.split();

    // The server's end of a byte stream, so `serve_client` is untouched: it
    // reads lines and writes lines, and this moves them across as messages.
    // Sized for one frame of a history load rather than one message.
    let (server_side, bridge_side) = tokio::io::duplex(256 * 1024);
    let serving = tokio::spawn(async move {
        if let Err(e) = crate::server::serve_client(server_side, hub, commands).await {
            log::debug!("web client disconnected: {e}");
        }
    });

    let (from_server, mut to_server) = tokio::io::split(bridge_side);

    // Two pumps rather than one `select!` over both directions.
    //
    // `read_line` is not cancellation safe: dropped part-way it can lose what
    // it had already taken out of the stream, and the next read would then
    // send the remainder of a frame as a whole message — a truncated JSON
    // object arriving at the page as if it were complete. A `select!` drops
    // exactly that future every time the other branch wins, which on a busy
    // connection is often. Each direction gets a task of its own instead, and
    // neither is ever cancelled mid-read.
    let mut to_client = tokio::spawn(async move {
        let mut from_server = BufReader::new(from_server);
        let mut line = String::new();
        loop {
            line.clear();
            match from_server.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    log::debug!("the server's end closed: {e}");
                    break;
                }
            }
            // The newline is the byte stream's framing; a WebSocket message
            // is already a frame, and sending it would put a stray newline
            // inside the JSON.
            let frame = line.trim_end_matches(['\r', '\n']);
            if frame.is_empty() {
                continue;
            }
            if outbound.send(Message::Text(frame.into())).await.is_err() {
                break;
            }
        }
        let _ = outbound.close().await;
    });

    let mut to_daemon = tokio::spawn(async move {
        while let Some(message) = inbound.next().await {
            match message {
                Ok(Message::Text(text)) => {
                    let mut framed = text.as_bytes().to_vec();
                    framed.push(b'\n');
                    if to_server.write_all(&framed).await.is_err() {
                        break;
                    }
                }
                // Ping and pong are answered by the library; a binary frame
                // is not something this protocol has.
                Ok(Message::Binary(_)) => {
                    log::warn!("ignoring a binary frame from a web client");
                }
                Ok(_) => {}
                Err(e) => {
                    log::debug!("web client framing error: {e}");
                    break;
                }
            }
        }
        // Dropping this end closes the server's, which ends its connection
        // task and in turn ends the pump above.
        drop(to_server);
    });

    // Whichever direction ends first ends the connection: a client that has
    // stopped reading is one the server cannot answer, and a server that has
    // finished has nothing left to say.
    tokio::select! {
        _ = &mut to_client => to_daemon.abort(),
        _ = &mut to_daemon => to_client.abort(),
    }
    serving.abort();
    Ok(())
}

/// Hand over one cached payload.
///
/// The same bytes `media_path` names for a front end that shares this
/// filesystem — a page does not, so it reads them over HTTP instead. The key
/// is validated by `media_path` itself, which is what keeps an echoed key
/// from naming a file outside the cache.
async fn serve_media(stream: &mut TcpStream, key: &str, origin: Option<&str>) -> Result<()> {
    let key = percent_decode(key);
    let Some(path) = oxidezap_ipc::media_path(&key) else {
        return respond(
            stream,
            400,
            "text/plain",
            origin,
            b"that is not a cache key",
        )
        .await;
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            // The daemon does not record what a payload was, and the front
            // end already knows: every one of these is named by a message
            // that carried its MIME type.
            respond(stream, 200, "application/octet-stream", origin, &bytes).await
        }
        Err(e) => {
            log::debug!("media {key} is not cached: {e}");
            respond(stream, 404, "text/plain", origin, b"not cached").await
        }
    }
}

/// One HTTP response, headers and all.
async fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    origin: Option<&str>,
    body: &[u8],
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        _ => "Not Found",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n",
        body.len()
    );
    // Echoed rather than `*`: the origin reaching here has already been
    // checked against what the user allowed, and naming it keeps the answer
    // scoped to the page that asked.
    if let Some(origin) = origin {
        head.push_str(&format!(
            "Access-Control-Allow-Origin: {origin}\r\n\
             Access-Control-Allow-Methods: GET, OPTIONS\r\n\
             Vary: Origin\r\n"
        ));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

/// Read up to the blank line that ends a request head.
async fn read_head(stream: &mut BufReader<TcpStream>) -> Result<String> {
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
struct Request {
    method: String,
    path: String,
    origin: Option<String>,
    websocket_key: Option<String>,
}

impl Request {
    fn parse(head: &str) -> Option<Self> {
        let mut lines = head.split("\r\n");
        let mut start = lines.next()?.split(' ');
        let method = start.next()?.to_string();
        // The query is not part of any route here, and a client is free to
        // add one — a cache buster, a token it thinks we want.
        let target = start.next()?;
        let path = target.split('?').next().unwrap_or(target).to_string();

        let mut origin = None;
        let mut websocket_key = None;
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
                _ => {}
            }
        }

        Some(Self {
            method,
            path,
            origin,
            websocket_key,
        })
    }

    /// Whether this origin is one the user said to serve.
    ///
    /// See the module documentation: this is the only thing standing between
    /// the session and any page that knows the port number.
    fn origin_allowed(&self, config: &Config) -> bool {
        let Some(origin) = &self.origin else {
            // Not a browser: a page cannot suppress `Origin`. So this is
            // something else on the machine, and it is served only where the
            // machine is the whole of the reach — a loopback bind, where
            // anything that can connect could equally have opened the Unix
            // socket. Off the loopback it is refused outright, because there
            // the header is the only check there is and a client that does
            // not send one cannot be subject to it.
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
}

/// Whether an origin is this machine talking to itself.
///
/// The developer's own `trunk serve`, which is the case that would otherwise
/// need configuring on every checkout. Matched on the host rather than by
/// prefix, so `https://localhost.example.com` is not mistaken for one.
fn is_loopback_origin(origin: &str) -> bool {
    let without_scheme = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .unwrap_or(origin);
    let host = without_scheme
        .split('/')
        .next()
        .unwrap_or(without_scheme)
        .rsplit_once(':')
        .map_or(without_scheme, |(host, _)| host);
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

/// The little decoding a media key can need.
///
/// The client encodes the key with `encodeURIComponent`; a key is already
/// restricted to characters that survive that untouched, so this exists for
/// the one that does not survive being *sent* — and anything malformed falls
/// through to `media_path`, which refuses it.
fn percent_decode(raw: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config(origins: &[&str]) -> Config {
        Config {
            addr: "127.0.0.1:0".parse().expect("a literal address"),
            allowed_origins: origins.iter().map(|o| (*o).to_string()).collect(),
        }
    }

    fn request(origin: Option<&str>) -> Request {
        Request {
            method: "GET".into(),
            path: WEB_SOCKET_PATH.into(),
            origin: origin.map(str::to_string),
            websocket_key: None,
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

    /// A client that sends no `Origin` is not a page — a page cannot suppress
    /// it — so it is served only where the reach is the machine itself.
    #[test]
    fn a_client_with_no_origin_is_served_only_on_loopback() {
        assert!(request(None).origin_allowed(&config(&[])));
        assert!(request(None).origin_allowed(&config(&["https://oxidezap.github.io"])));
    }

    /// Off the loopback the header is the only check there is, and a client
    /// that sends none cannot be subject to it. Refused rather than trusted.
    #[test]
    fn a_client_with_no_origin_is_refused_off_the_loopback() {
        let exposed = Config {
            addr: "0.0.0.0:9527".parse().expect("a literal address"),
            allowed_origins: vec!["https://oxidezap.github.io".to_string()],
        };
        assert!(!request(None).origin_allowed(&exposed));
        // A named origin still is, which is the whole point of naming one.
        assert!(request(Some("https://oxidezap.github.io")).origin_allowed(&exposed));
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

    /// The example from RFC 6455 §1.3, which is what proves the handshake
    /// answer is the one a browser will accept.
    #[test]
    fn the_accept_key_is_the_one_rfc6455_specifies() {
        let mut hasher = Sha1::new();
        hasher.update(b"dGhlIHNhbXBsZSBub25jZQ==");
        hasher.update(WS_MAGIC.as_bytes());
        let accept = base64::engine::general_purpose::STANDARD.encode(hasher.finalize());
        assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }
}
