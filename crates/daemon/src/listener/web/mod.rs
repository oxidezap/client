//! The daemon's third endpoint: the one a browser can reach.
//!
//! A Unix socket and a named pipe are both things only a process on this
//! machine can open, which is exactly why they were chosen — the endpoint
//! carries control of a WhatsApp session. A page can open neither. So a front
//! end that is a tab gets a TCP port on loopback speaking the same
//! newline-delimited JSON over a WebSocket, plus the media sideband that
//! protocol depends on, served over HTTP beside it.
//!
//! # What is here and what is not
//!
//! The transport, and nothing else — which is what /AGENTS.md asks of this
//! directory. This module accepts, admits and upgrades; [`http`] is the
//! request head it reads and the responses it writes, including the two
//! checks every caller passes before a route is chosen. The media sideband
//! itself is not a transport and lives with the rest of media, in
//! [`crate::media::http`]: this one routes to it. The token those checks
//! compare against is a file in the per-user directory rather than anything
//! about a socket, so it is drawn and read in [`crate::private_dir`] and
//! re-exported here as [`token`] for the binary that starts this.
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
//! `trunk serve`. The token is the admission check; the origin only narrows
//! who can probe. A request carrying no `Origin` is not necessarily something
//! other than a browser — an `<img>`, a `<script>` or a form GET sends none —
//! so it is served on a loopback bind and still only with the token.

/// The HTTP this port speaks before the upgrade, and the admission checks it
/// carries.
pub(crate) mod http;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine as _;
use futures_util::{SinkExt as _, StreamExt as _};
use oxidezap_ipc::{WEB_MEDIA_PATH, WEB_SOCKET_PATH};
use sha1::{Digest as _, Sha1};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};

use self::http::{HEAD_TIMEOUT, Request, preflight, read_head, respond};
use crate::server::{self, ClientSlots};
use crate::session_bridge::Commands;
use crate::state::StateHub;

/// The bridge's shared secret, drawn once and kept in the per-user directory.
///
/// Here because this is where it is checked and where the binary asks for it;
/// written and read where the rest of that directory's rules live.
pub use crate::private_dir::web_token as token;

/// What the bridge was asked to be.
#[derive(Clone, Debug)]
pub struct Config {
    /// Where to listen. Loopback unless the user said otherwise.
    pub addr: SocketAddr,
    /// Browser origins allowed to attach, beyond the loopback ones.
    pub allowed_origins: Vec<String>,
    /// The shared secret every request has to carry.
    ///
    /// See [`token`]: this is what makes a machine-wide port as private as
    /// the per-user socket beside it.
    pub token: String,
}

/// The magic string RFC 6455 mixes into the accept key. Not a secret: it
/// exists so a cache or a proxy cannot be tricked into completing a
/// handshake it did not understand.
const WS_MAGIC: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// How many connections may be *waiting to say who they are*.
///
/// [`ClientSlots`] counts front ends, and a front end is something that has
/// already presented a token and completed an upgrade. Before that, every
/// accepted socket costs a task and a descriptor for up to [`HEAD_TIMEOUT`],
/// and nothing had bounded how many of those there could be: a loop opening
/// connections and saying nothing would take the process's descriptors down
/// with it, and the IPC endpoint beside it with them.
///
/// Generous, because this is also the media path and a page fetching a
/// screenful of photos opens several at once. It is a ceiling on a stall,
/// not a rate limit.
const MAX_PENDING: usize = 128;

/// Serve until the future is dropped.
///
/// # Errors
///
/// The port could not be bound. Per-connection failures are logged and
/// dropped, exactly as the IPC listener treats them.
pub async fn run(
    config: Config,
    hub: Arc<StateHub>,
    plugins: Arc<oxidezap_plugin_host::Plugins>,
    commands: Commands,
    slots: ClientSlots,
) -> Result<()> {
    // Loopback only, and refused rather than warned about.
    //
    // Off this machine the `Origin` header is not a check at all — it is a
    // string the client chooses, and a program that is not a browser writes
    // whatever it likes there — so the one thing standing between the network
    // and a WhatsApp session would be a request to please identify honestly.
    // The traffic is also plain TCP: no TLS a browser would accept is
    // something a daemon can produce for itself.
    //
    // Reaching this from another machine is a tunnel's job (`ssh -L`, or a
    // reverse proxy that terminates TLS and authenticates), which is a
    // deliberate act by someone who can see what they are exposing.
    if !config.addr.ip().is_loopback() {
        anyhow::bail!(
            "the web bridge refuses to bind {}: off the loopback its only check is an \
             `Origin` header the client chooses, and the session would cross the network \
             in the clear. Bind it to 127.0.0.1 and reach it through a tunnel.",
            config.addr
        );
    }

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
    // The path, not the token. It is a bearer credential: anything that has
    // it is this user as far as the endpoint is concerned, and a log is the
    // one artefact people paste into issues. The file it names is the
    // restricted channel already — `0600`, in the user's own directory — so
    // saying where it is tells the person who may read it everything and
    // tells a log reader nothing.
    if let Some(path) = oxidezap_ipc::web_token_path() {
        log::info!(
            "point a page at #daemon=ws://{}{WEB_SOCKET_PATH}?token=<token>, \
             where <token> is the contents of {}",
            config.addr,
            path.display()
        );
    }

    // Held across the whole of `serve`, which covers the head, the token and
    // the response — and, for a client that upgrades, the connection itself.
    // A slot released at the upgrade would let the stall this bounds happen
    // one upgrade later.
    let pending = Arc::new(tokio::sync::Semaphore::new(MAX_PENDING));

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                log::warn!("skipping a web connection we could not accept: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        // Taken before the task exists rather than inside it: a permit
        // acquired in the spawned task would mean the task, and the socket it
        // holds, already existed — which is the thing being bounded.
        let Ok(permit) = Arc::clone(&pending).try_acquire_owned() else {
            log::warn!("refusing a web connection: {MAX_PENDING} are already waiting to identify");
            drop(stream);
            continue;
        };
        let config = config.clone();
        let hub = Arc::clone(&hub);
        let plugins = Arc::clone(&plugins);
        let commands = commands.clone();
        let slots = Arc::clone(&slots);
        tokio::spawn(async move {
            if let Err(e) = serve(stream, &config, hub, plugins, commands, slots).await {
                log::debug!("web client {peer} disconnected: {e}");
            }
            drop(permit);
        });
    }
}

/// One connection: a WebSocket upgrade, a media request, or a refusal.
async fn serve(
    stream: TcpStream,
    config: &Config,
    hub: Arc<StateHub>,
    plugins: Arc<oxidezap_plugin_host::Plugins>,
    commands: Commands,
    slots: ClientSlots,
) -> Result<()> {
    let mut stream = BufReader::new(stream);
    let head = match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut stream)).await {
        Ok(head) => head?,
        Err(_) => anyhow::bail!("no request within {HEAD_TIMEOUT:?}"),
    };
    let request = Request::parse(&head).context("unparsable request")?;

    // The one thing checked before anything else is served — and answered
    // the same way a bad token is, which the comment here used to describe
    // and the code did not do.
    //
    // A `403` saying "this daemon was not told to accept that origin" is a
    // confirmation that a daemon is here, handed to a caller who has not
    // authenticated and cannot. Every account on the machine can reach this
    // port, so that turns the origin check into a discovery oracle for the
    // one thing the token exists to keep private. Wrong origin, wrong token
    // and nothing listening now look alike.
    if !request.origin_allowed(config) {
        log::warn!(
            "refusing a web client from origin {:?}",
            request.origin.as_deref().unwrap_or("(none)")
        );
        return respond(stream.get_mut(), 404, "text/plain", None, b"nothing here").await;
    }
    let origin = request.origin.clone();

    // Before every route and before the preflight, which is a route like any
    // other: a `204` to an `OPTIONS` nobody could have authenticated is this
    // endpoint confirming it is here to an account that may not open it. A
    // browser sends the preflight to the same URL as the request it is
    // asking about, token and all, so there is nothing to exempt.
    //
    // This is what makes the port as private as the socket beside it. A `404`
    // rather than a `403`: an endpoint the caller may not open has no reason
    // to say it exists.
    if !request.token_matches(config) {
        log::warn!("refusing a web request with no valid token");
        return respond(
            stream.get_mut(),
            404,
            "text/plain",
            origin.as_deref(),
            b"nothing here",
        )
        .await;
    }

    // A browser asks before it fetches media from another origin.
    if request.method == "OPTIONS" {
        return preflight(
            stream.get_mut(),
            origin.as_deref(),
            request.wants_private_network,
        )
        .await;
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
            return refuse_full(stream, &key).await;
        };
        let served = attach(stream, &key, hub, plugins, commands).await;
        drop(slot);
        return served;
    }

    if let Some(key) = request.path.strip_prefix(&format!("{WEB_MEDIA_PATH}/")) {
        // A staging upload is the one write this endpoint takes, and it reads
        // from the buffered reader rather than the socket: the head was read
        // a byte at a time but a client may send head and body in one
        // segment, so the first of the payload can already be sitting in the
        // buffer. Reading the raw stream would drop exactly that much.
        //
        // `PUT` and nothing else, because the preflight in [`http`]
        // advertises exactly this list: taking a method the browser is told it may not
        // use is a route no page can reach, and one the next reader has to
        // work out is dead.
        if request.method == "PUT" {
            let key = key.to_string();
            return crate::media::http::receive(
                &mut stream,
                &key,
                origin.as_deref(),
                request.content_length,
            )
            .await;
        }
        // The other half of staging: a send abandoned before the request ran
        // leaves a payload nothing will ever read, and staged uploads are
        // deliberately spared by the cache sweep, so without this they stay
        // until the account is wiped. Narrowed the same way the write is.
        if request.method == "DELETE" {
            return crate::media::http::discard(stream.get_mut(), key, origin.as_deref()).await;
        }
        return crate::media::http::serve(stream.get_mut(), key, origin.as_deref()).await;
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
/// Answer the handshake and take over the socket.
///
/// Separate from [`attach`] because refusing a client has to get this far
/// too: the refusal is a protocol frame, and a protocol frame is only
/// readable by a page once the upgrade has completed.
async fn upgrade(
    mut stream: BufReader<TcpStream>,
    key: &str,
) -> Result<WebSocketStream<TcpStream>> {
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
    //
    // Sized to the same budget the server enforces on a frame. The library's
    // defaults are 64 MiB per message and 16 MiB per frame, which would have
    // this end assemble a message sixty times larger than anything the server
    // will accept before the server got to refuse it — the allocation is the
    // cost, and it happens here.
    let config = WebSocketConfig::default()
        .max_message_size(Some(oxidezap_ipc::MAX_REQUEST_BYTES))
        .max_frame_size(Some(oxidezap_ipc::MAX_REQUEST_BYTES));
    Ok(WebSocketStream::from_raw_socket(stream.into_inner(), Role::Server, Some(config)).await)
}

/// Tell a page the daemon is full, in a way it can actually read.
///
/// The obvious refusal — writing the protocol's own error frame onto the
/// stream, as the socket listener does — is bytes on a connection the browser
/// still believes is an HTTP request awaiting its `101`. It reads them as a
/// malformed handshake and reports the opaque failure it reports for every
/// other one, so the page retries forever against a daemon that will keep
/// saying no.
///
/// An HTTP `503` would be well-formed and no more use: the WebSocket API
/// deliberately hides a failed handshake's status and body from the page.
/// What a page *can* read is a message on an open socket, so the upgrade is
/// completed for the sole purpose of saying one word and hanging up.
async fn refuse_full(stream: BufReader<TcpStream>, key: &str) -> Result<()> {
    log::warn!(
        "refusing a web client: already serving {}",
        server::MAX_CLIENTS
    );
    let mut socket = upgrade(stream, key).await?;
    if let Ok(frame) = server::too_many_clients_frame() {
        let _ = socket.send(Message::Text(frame.into())).await;
    }
    let _ = socket.close(None).await;
    Ok(())
}

async fn attach(
    stream: BufReader<TcpStream>,
    key: &str,
    hub: Arc<StateHub>,
    plugins: Arc<oxidezap_plugin_host::Plugins>,
    commands: Commands,
) -> Result<()> {
    let socket = upgrade(stream, key).await?;
    let (mut outbound, mut inbound) = socket.split();

    // The server's end of a byte stream, so `serve_client` is untouched: it
    // reads lines and writes lines, and this moves them across as messages.
    // Sized for one frame of a history load rather than one message.
    let (server_side, bridge_side) = tokio::io::duplex(256 * 1024);
    let serving = tokio::spawn(async move {
        if let Err(e) = crate::server::serve_client(server_side, hub, plugins, commands).await {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A bind that is not loopback is refused before anything is served, so
    /// the `Origin` header never has to carry weight it cannot bear.
    #[tokio::test]
    async fn a_bind_off_the_loopback_is_refused() {
        let exposed = Config {
            addr: "0.0.0.0:0".parse().expect("a literal address"),
            allowed_origins: vec!["https://oxidezap.github.io".to_string()],
            token: http::tests::TEST_TOKEN.to_string(),
        };
        let hub = StateHub::new();
        let (commands, _rx) = tokio::sync::mpsc::channel(1);
        let plugins = Arc::new(oxidezap_plugin_host::Plugins::nothing_loaded(Arc::new(
            |_| {},
        )));
        let refused = run(exposed, hub, plugins, commands, server::client_slots()).await;
        let message = refused
            .expect_err("a non-loopback bind is refused")
            .to_string();
        assert!(
            message.contains("refuses to bind"),
            "refused for the wrong reason: {message}"
        );
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
