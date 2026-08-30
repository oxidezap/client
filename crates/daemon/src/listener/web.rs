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
//! `trunk serve`. The token is the admission check; the origin only narrows
//! who can probe. A request carrying no `Origin` is not necessarily something
//! other than a browser — an `<img>`, a `<script>` or a form GET sends none —
//! so it is served on a loopback bind and still only with the token.

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
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};

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

/// How long a connection has to send its request line and headers.
///
/// Small: this is a local socket and the whole head is one packet. A peer
/// that connects and says nothing otherwise holds a task for as long as it
/// likes, which is the same reason the IPC handshake is bounded.
const HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The most a request head may be before it is refused.
const MAX_HEAD: usize = 16 * 1024;

/// The most a front end may stage in one payload.
///
/// This one *is* read into the daemon's memory, unlike a served file, because
/// the write has to be a single act: a partly staged payload under a key a
/// send is about to name is worse than a refused one. So the ceiling is what
/// keeps that from being unbounded, and it is sized for what actually goes
/// through here — a voice note or a photo, never a film.
const MAX_UPLOAD_BYTES: u64 = 64 * 1024 * 1024;

/// How long a staged payload has to arrive once its head has been read.
///
/// The head has [`HEAD_TIMEOUT`]; the body needs its own, and for a sharper
/// reason: this read holds a [`MAX_PENDING`] permit *and* a buffer of the
/// declared size while it waits. A client killed mid-upload — no attacker
/// required — otherwise parks both until the process ends, and enough of them
/// leave the daemon refusing every new connection including a front end's.
const BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How many payloads may be in memory at once.
///
/// The per-payload ceiling bounds one upload and [`MAX_PENDING`] bounds the
/// connections, so without this the product is what the process holding the
/// account can be made to hold. `serve_media` streams for exactly this reason
/// and staging cannot, so it gets a bound instead.
const MAX_CONCURRENT_UPLOADS: usize = 4;

/// The permits [`MAX_CONCURRENT_UPLOADS`] hands out.
static UPLOAD_SLOTS: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(MAX_CONCURRENT_UPLOADS);

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
        // `PUT` and nothing else, because the preflight below advertises
        // exactly this list: taking a method the browser is told it may not
        // use is a route no page can reach, and one the next reader has to
        // work out is dead.
        if request.method == "PUT" {
            let key = key.to_string();
            return receive_media(&mut stream, &key, origin.as_deref(), request.content_length)
                .await;
        }
        // The other half of staging: a send abandoned before the request ran
        // leaves a payload nothing will ever read, and staged uploads are
        // deliberately spared by the cache sweep, so without this they stay
        // until the account is wiped. Narrowed the same way the write is.
        if request.method == "DELETE" {
            return discard_media(stream.get_mut(), key, origin.as_deref()).await;
        }
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
    // Opened rather than read. A video is tens of megabytes and this process
    // is also the one holding the WhatsApp session — reading each request's
    // payload whole would let a handful of tabs fetching attachments at once
    // put several films on the daemon's heap and take the account down with
    // them. The length comes from the metadata, so the head is still exact.
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(e) => {
            log::debug!("media {key} is not cached: {e}");
            return respond(stream, 404, "text/plain", origin, b"not cached").await;
        }
    };
    let length = match file.metadata().await {
        Ok(metadata) => metadata.len(),
        Err(e) => {
            log::debug!("media {key} could not be measured: {e}");
            return respond(stream, 404, "text/plain", origin, b"not cached").await;
        }
    };

    // The daemon does not record what a payload was, and the front end
    // already knows: every one of these is named by a message that carried
    // its MIME type.
    let mut head = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/octet-stream\r\n\
         Content-Length: {length}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n"
    );
    if let Some(origin) = origin {
        head.push_str(&format!(
            "Access-Control-Allow-Origin: {origin}\r\n\
             Access-Control-Allow-Methods: GET, PUT, DELETE, OPTIONS\r\n\
             Vary: Origin\r\n"
        ));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;

    let mut file = tokio::io::BufReader::new(file);
    tokio::io::copy(&mut file, stream).await?;
    stream.flush().await?;
    Ok(())
}

/// Take a payload a front end staged for a send it is about to ask for.
///
/// The mirror of [`serve_media`], and deliberately not its equal. A page has
/// no filesystem, so a voice note it recorded exists only in its own memory
/// until the daemon can read it — and the daemon reads payloads from disk,
/// because `SendAudio` names a key rather than carrying bytes. This is the
/// only way those two facts meet.
///
/// Three things narrow it, because a write endpoint on the process holding
/// the account deserves more than a read one:
///
/// * Only `u-` keys. `f-` and `d-` are the daemon's own cache of things it
///   fetched and can fetch again; letting a caller write those would let one
///   replace the bytes of a photo already on screen. `u-` is a payload whose
///   only copy is the one being sent, and nothing else writes there.
/// * A ceiling, checked against the declared length *before* anything is
///   read, so an oversized upload costs a header rather than a disk.
/// * The token, which the caller has already passed to get here.
async fn receive_media(
    stream: &mut BufReader<TcpStream>,
    key: &str,
    origin: Option<&str>,
    length: Option<u64>,
) -> Result<()> {
    let key = percent_decode(key);
    if let Some((status, reason)) = staging_refusal(&key, length) {
        log::warn!("refusing an upload to {key}: {reason}");
        return respond(
            stream.get_mut(),
            status,
            "text/plain",
            origin,
            reason.as_bytes(),
        )
        .await;
    }
    let Some(path) = oxidezap_ipc::media_path(&key) else {
        return respond(
            stream.get_mut(),
            400,
            "text/plain",
            origin,
            b"that is not a cache key",
        )
        .await;
    };
    // Checked above; named here because the read below needs the number.
    let length = length.unwrap_or(0);

    // One at a time, up to the bound: this is the read that holds a payload in
    // the daemon's memory, and the permit is taken before the buffer exists.
    let Ok(_slot) = UPLOAD_SLOTS.try_acquire() else {
        log::warn!("refusing an upload to {key}: too many already in flight");
        return respond(
            stream.get_mut(),
            503,
            "text/plain",
            origin,
            b"too many uploads in flight",
        )
        .await;
    };

    // Exactly the declared length, so a client that promises less than it
    // sends leaves the surplus in the socket rather than in the file, and one
    // that promises more is cut off by the read rather than trusted.
    let mut body = vec![0u8; usize::try_from(length).unwrap_or(0)];
    let read = tokio::time::timeout(BODY_TIMEOUT, stream.read_exact(&mut body)).await;
    if let Err(e) = read.map_err(std::io::Error::from).and_then(|inner| inner) {
        log::warn!("an upload to {key} ended early: {e}");
        return respond(
            stream.get_mut(),
            400,
            "text/plain",
            origin,
            b"the payload ended early",
        )
        .await;
    }

    if let Some(dir) = path.parent()
        && let Err(e) = tokio::fs::create_dir_all(dir).await
    {
        log::error!("could not make room for {key}: {e}");
        return respond(
            stream.get_mut(),
            500,
            "text/plain",
            origin,
            b"could not stage that payload",
        )
        .await;
    }
    // Written beside the target and renamed onto it. Reading the whole body
    // first stops a short *client* from leaving a partial file; it does not
    // stop a crash or a full disk part way through the write, and what that
    // leaves is a valid-looking key holding a truncated voice note, which the
    // daemon then opens when it handles the send. A rename within one
    // directory is atomic, so the key holds the whole payload or nothing.
    // Named so that no key can address it: `media_path` refuses a leading
    // dot, so this file is not something a caller can read half-written or
    // delete out from under the rename. The counter keeps two uploads of one
    // key from writing over each other's temporary file.
    let partial = path.with_file_name(format!(
        ".staging-{}-{key}",
        STAGING_SEQUENCE.fetch_add(1, portable_atomic::Ordering::Relaxed)
    ));
    let staged = async {
        tokio::fs::write(&partial, &body).await?;
        tokio::fs::rename(&partial, &path).await
    }
    .await;
    if let Err(e) = staged {
        log::error!("could not stage {key}: {e}");
        let _ = tokio::fs::remove_file(&partial).await;
        return respond(
            stream.get_mut(),
            500,
            "text/plain",
            origin,
            b"could not stage that payload",
        )
        .await;
    }
    log::debug!("staged {} bytes under {key}", body.len());
    respond(stream.get_mut(), 204, "text/plain", origin, b"").await
}

/// Drop a staged payload whose send is not going to happen.
///
/// Only `u-` keys, for the reason the write takes only those: the daemon's
/// own cache is not a caller's to delete either. Silent about a key that is
/// not there, because the caller is discarding rather than asking.
async fn discard_media(stream: &mut TcpStream, key: &str, origin: Option<&str>) -> Result<()> {
    let key = percent_decode(key);
    if !is_staged(&key) {
        log::warn!("refusing to discard {key}: only staged payloads may be removed");
        return respond(
            stream,
            403,
            "text/plain",
            origin,
            b"only staged payloads may be removed",
        )
        .await;
    }
    if let Some(path) = oxidezap_ipc::media_path(&key) {
        match tokio::fs::remove_file(&path).await {
            Ok(()) => log::debug!("discarded the staged payload {key}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("could not discard {key}: {e}"),
        }
    }
    respond(stream, 204, "text/plain", origin, b"").await
}

/// Why this staging request may not proceed, if it may not.
///
/// Pure, and separate from the handler, because these three are the whole
/// authorization story for the one route on this endpoint that *writes*: the
/// prefix is what keeps a caller out of the daemon's own cache, and the
/// length is what keeps an upload from being unbounded. A guard worth having
/// is a guard worth testing without a socket.
/// Whether this key names a payload a caller staged, and may therefore write
/// or remove.
///
/// `f-` and `d-` are the daemon's own cache of what it fetched and can fetch
/// again; those are not a caller's to replace or delete.
/// Distinguishes one in-flight staging write from another.
static STAGING_SEQUENCE: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);

fn is_staged(key: &str) -> bool {
    oxidezap_ipc::is_staged_key(key)
}

fn staging_refusal(key: &str, length: Option<u64>) -> Option<(u16, &'static str)> {
    // `f-` and `d-` are the daemon's cache of what it fetched and can fetch
    // again; writing those would let a caller replace the bytes behind a
    // photo already on screen. `u-` is a payload whose only copy is the one
    // being sent, and nothing else writes there.
    if !is_staged(key) {
        return Some((403, "only staged payloads may be written"));
    }
    let Some(length) = length else {
        return Some((411, "a staged payload must declare its length"));
    };
    // Before a byte is read, so an oversized upload costs a header rather
    // than a disk.
    if length > MAX_UPLOAD_BYTES {
        return Some((413, "that payload is too large to stage"));
    }
    None
}

/// One HTTP response, headers and all.
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
async fn preflight(
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
    /// Everything after the `?`, which is where the token rides.
    query: String,
    origin: Option<String>,
    websocket_key: Option<String>,
    /// Whether this preflight is asking to reach a private address, which is
    /// what a page served from a public origin has to ask before it may talk
    /// to loopback at all.
    wants_private_network: bool,
    /// The declared body length, which only a staging upload has.
    ///
    /// Read rather than trusted: it decides how much is read, so
    /// [`receive_media`] refuses one past its ceiling before a byte arrives
    /// rather than discovering the size by accepting it.
    content_length: Option<u64>,
}

impl Request {
    fn parse(head: &str) -> Option<Self> {
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
    fn origin_allowed(&self, config: &Config) -> bool {
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
    fn token_matches(&self, config: &Config) -> bool {
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

    /// A token the tests can also write into a request.
    pub(super) const TEST_TOKEN: &str = "0123456789abcdef";

    fn config(origins: &[&str]) -> Config {
        Config {
            addr: "127.0.0.1:0".parse().expect("a literal address"),
            allowed_origins: origins.iter().map(|o| (*o).to_string()).collect(),
            token: TEST_TOKEN.to_string(),
        }
    }

    fn request(origin: Option<&str>) -> Request {
        Request {
            method: "GET".into(),
            path: WEB_SOCKET_PATH.into(),
            query: format!("token={TEST_TOKEN}"),
            origin: origin.map(str::to_string),
            websocket_key: None,
            wants_private_network: false,
            content_length: None,
        }
    }

    /// The daemon's own cache is not writable through the bridge. Without
    /// this a caller holding the token could replace the bytes behind a photo
    /// already drawn, which is a different power from staging one to send.
    #[test]
    fn only_staged_keys_may_be_written() {
        for key in ["f-abc", "d-abc", "abc", "approvals", ".."] {
            assert_eq!(
                staging_refusal(key, Some(16)).map(|(status, _)| status),
                Some(403),
                "{key} should not be writable"
            );
        }
        assert_eq!(staging_refusal("u-abc", Some(16)), None);
    }

    /// The length decides how much is read, so a request without one has no
    /// bound to be held to.
    #[test]
    fn a_staged_payload_declares_its_length() {
        assert_eq!(
            staging_refusal("u-abc", None).map(|(status, _)| status),
            Some(411)
        );
    }

    /// Refused from the header rather than discovered by accepting it: this
    /// payload is read into the process holding the account.
    #[test]
    fn an_oversized_payload_is_refused_before_it_is_read() {
        assert_eq!(
            staging_refusal("u-abc", Some(MAX_UPLOAD_BYTES + 1)).map(|(status, _)| status),
            Some(413)
        );
        assert_eq!(staging_refusal("u-abc", Some(MAX_UPLOAD_BYTES)), None);
    }

    /// A discard is narrowed the same way a write is: the daemon's own cache
    /// is not a caller's to delete either.
    #[test]
    fn only_staged_keys_may_be_discarded() {
        for key in ["f-abc", "d-abc", "abc", "approvals"] {
            assert!(!is_staged(key), "{key} should not be removable");
        }
        assert!(is_staged("u-abc"));
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

    /// A bind that is not loopback is refused before anything is served, so
    /// the `Origin` header never has to carry weight it cannot bear.
    #[tokio::test]
    async fn a_bind_off_the_loopback_is_refused() {
        let exposed = Config {
            addr: "0.0.0.0:0".parse().expect("a literal address"),
            allowed_origins: vec!["https://oxidezap.github.io".to_string()],
            token: TEST_TOKEN.to_string(),
        };
        let hub = StateHub::new();
        let (commands, _rx) = tokio::sync::mpsc::channel(1);
        let plugins = Arc::new(oxidezap_plugin_host::Plugins::none(Arc::new(|_| {})));
        let refused = run(exposed, hub, plugins, commands, server::client_slots()).await;
        let message = refused
            .expect_err("a non-loopback bind is refused")
            .to_string();
        assert!(
            message.contains("refuses to bind"),
            "refused for the wrong reason: {message}"
        );
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

/// How long a drawn token is, in characters.
///
/// 24 bytes as lowercase hex. Named rather than spelled twice: the draw and
/// the check below have to agree, and a check that had drifted would either
/// refuse every token or accept a truncated one.
const TOKEN_CHARS: usize = 48;

/// Whether this is a value this daemon could have written.
///
/// The whole of the shape, because the whole of it is what makes the token
/// unguessable: a prefix of one is a shorter secret, not a valid one.
fn is_a_token(value: &str) -> bool {
    value.len() == TOKEN_CHARS
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The bridge's shared secret, created on first use and reused after.
///
/// Reused rather than redrawn per run so a bookmarked URL keeps working
/// across restarts — a token nobody can remember is one that gets turned off.
/// Written into the same per-user directory as the socket and the lock, with
/// no access for anyone else, because the whole point of it is that another
/// account on this machine cannot read it.
///
/// # Errors
///
/// No per-user directory, or the file could not be read or written.
pub fn token() -> Result<String> {
    let path = oxidezap_ipc::web_token_path().context("no per-user directory for the web token")?;

    if let Some(existing) = read_private(&path)? {
        let existing = existing.trim();
        if is_a_token(existing) {
            return Ok(existing.to_string());
        }
        if !existing.is_empty() {
            // Ours by owner and mode, and still not a token. A partial write
            // is how that happens — a full disk answers `write_all` with an
            // error after some of the bytes have landed — and what it leaves
            // is a *short* credential, which is the one kind of malformed
            // value that is worse than none: the endpoint's whole admission
            // check is this string, and another local account can reach the
            // port and try. Redrawn rather than refused, because the file is
            // this user's own and a daemon that will not start over a
            // truncated byte is a daemon nobody can recover.
            log::warn!(
                "{} did not hold a well-formed token; drawing a new one",
                path.display()
            );
        }
    }

    // 192 bits, hex. Not a password: nobody types it, it is pasted, so it is
    // sized to be unguessable rather than to be short.
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).context("no randomness for the web token")?;
    let mut drawn = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(drawn, "{byte:02x}");
    }

    write_private(&path, &drawn)
        .with_context(|| format!("writing the web token to {}", path.display()))?;
    Ok(drawn)
}

/// What is wrong with this file, if anything, in the one sense that matters.
///
/// The three things "we wrote this" means: a regular file, owned by this
/// user, readable by nobody else. One place decides it, because the read side
/// and the write side have to agree — a check that admitted on one and not
/// the other would be a token written into a file the next run refuses, or
/// worse, the reverse.
///
/// Asked of a `Metadata` rather than a path so it can be asked of an open
/// descriptor: what is checked has to be what is used, or the answer is about
/// a file that was there a moment ago.
#[cfg(unix)]
fn not_ours(about: &std::fs::Metadata) -> Option<&'static str> {
    use std::os::unix::fs::MetadataExt as _;

    let us = rustix::process::getuid().as_raw();
    if !about.is_file() {
        Some("not a regular file")
    } else if about.uid() != us {
        Some("owned by another user")
    } else if about.mode() & 0o077 != 0 {
        Some("readable by other users")
    } else {
        None
    }
}

/// Read the token back, refusing anything that is not the file we wrote.
///
/// A bearer credential is only worth the guarantee that nobody else can name
/// it, and following a symlink hands that guarantee to whoever planted the
/// link: the directory is `0700` today, but an installation that predates
/// that — or one whose state directory somebody widened — can already have a
/// `web.token` pointing at a file another account controls. Read through it
/// and the daemon comes up with a bearer token that account knows, which is
/// the whole session.
///
/// So the link is not followed, and the file it would have been is checked
/// for the three things "we wrote this" means: a regular file, owned by this
/// user, readable by nobody else. Anything else is an error rather than a
/// token redrawn over it — writing would follow the same link, which is the
/// worse half of the same problem, and a state directory in that condition is
/// something a person has to look at.
///
/// # Errors
///
/// The path is there and is not ours. Absent is not an error: that is the
/// first run, and the caller draws one.
#[cfg(unix)]
fn read_private(path: &std::path::Path) -> Result<Option<String>> {
    use std::io::Read as _;

    let opened = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    );
    let fd = match opened {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        // `ELOOP` is precisely the case this exists for: something is there
        // and it is a symlink. Named rather than folded into the rest,
        // because the rest are ordinary I/O failures and this one is not.
        Err(rustix::io::Errno::LOOP) => anyhow::bail!(
            "{} is a symbolic link. The web token is a bearer credential and this one would be \
             somebody else's file; remove it and start again.",
            path.display()
        ),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("opening {}", path.display()))),
    };

    let mut file = std::fs::File::from(fd);
    let about = file
        .metadata()
        .with_context(|| format!("reading the type and owner of {}", path.display()))?;
    if let Some(wrong) = not_ours(&about) {
        anyhow::bail!(
            "{} is {wrong}. The web token is a bearer credential for this account's WhatsApp \
             session; remove it and start again.",
            path.display()
        );
    }

    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(contents))
}

/// The same, where the directory is already inside the user's own profile.
///
/// There is no other account to defend against on the path a Windows profile
/// takes, which is the same reason [`write_private`] has nothing to set there.
#[cfg(not(unix))]
fn read_private(path: &std::path::Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context(format!("reading {}", path.display()))),
    }
}

/// Create the token file readable by nobody else.
///
/// The mode is set as the file is created rather than after: a token that is
/// briefly world-readable is a token another account had a moment to read.
/// Which is also why the create is exclusive. `create(true)` opens whatever
/// is already at the path, and `mode` is only consulted when a file is made —
/// so a regular file another account planted between [`read_private`]
/// answering "nothing there" and this call is a file we would open, leave at
/// its owner's mode, and write this session's bearer token into. `O_NOFOLLOW`
/// does not cover it: nothing about that is a symlink.
///
/// The path can legitimately exist, though — the empty file a previous run
/// left, which is the case the caller redraws over — so `EEXIST` is not the
/// end. It reopens without creating and asks the *descriptor* the three
/// questions [`not_ours`] asks, which is the only form of the question that
/// cannot be answered about a different file than the one written to.
#[cfg(unix)]
fn write_private(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    // `O_NOFOLLOW` throughout, for the reason [`read_private`] does not
    // follow one either: writing through a planted link would put this
    // account's bearer token into a file somebody else can read, which is
    // worse than reading theirs.
    let nofollow = rustix::fs::OFlags::NOFOLLOW.bits() as i32;
    let made = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nofollow)
        .open(path);
    let mut file = match made {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Not truncating on the way in: what is there is unexamined until
            // the descriptor answers for itself, and destroying it first
            // would be doing an attacker's file a favour as readily as our
            // own.
            let file = std::fs::OpenOptions::new()
                .write(true)
                .truncate(false)
                .custom_flags(nofollow)
                .open(path)?;
            let about = file.metadata()?;
            if let Some(wrong) = not_ours(&about) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "{} is {wrong}. The web token is a bearer credential for this account's \
                         WhatsApp session; remove it and start again.",
                        path.display()
                    ),
                ));
            }
            file.set_len(0)?;
            file
        }
        Err(e) => return Err(e),
    };
    file.write_all(contents.as_bytes())
}

/// The same, where the directory is already inside the user's own profile.
#[cfg(not(unix))]
fn write_private(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

#[cfg(test)]
mod token_tests {
    use super::tests::TEST_TOKEN;
    use super::*;

    fn asking(query: &str) -> Request {
        Request {
            method: "GET".into(),
            path: WEB_SOCKET_PATH.into(),
            query: query.to_string(),
            origin: None,
            websocket_key: None,
            wants_private_network: false,
            content_length: None,
        }
    }

    fn config() -> Config {
        Config {
            addr: "127.0.0.1:0".parse().expect("a literal address"),
            allowed_origins: Vec::new(),
            token: TEST_TOKEN.to_string(),
        }
    }

    /// What the token file has to be before it is believed.
    ///
    /// The link case is the one with teeth: a state directory that predates
    /// the `0700` rule, or one somebody widened, can already hold a
    /// `web.token` pointing somewhere another account writes — and following
    /// it would hand that account a bearer token for this WhatsApp session.
    #[cfg(unix)]
    #[test]
    fn a_token_file_that_is_not_ours_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("oxidezap-token-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a directory to test in");

        // Absent is not a failure: it is the first run.
        assert!(
            read_private(&dir.join("missing"))
                .expect("absent is not an error")
                .is_none()
        );

        // Ours, and private: read back verbatim.
        let ours = dir.join("web.token");
        write_private(&ours, "0123456789abcdef").expect("write the token");
        assert_eq!(
            read_private(&ours).expect("ours reads back").as_deref(),
            Some("0123456789abcdef")
        );

        // A link, however inviting what it points at.
        let planted = dir.join("planted");
        std::fs::write(&planted, "attacker-known").expect("the file it points at");
        let link = dir.join("linked.token");
        std::os::unix::fs::symlink(&planted, &link).expect("plant the link");
        assert!(read_private(&link).is_err(), "a symlink must be refused");
        // And the write refuses it too, which is the half that would have
        // leaked our own token rather than adopted theirs.
        assert!(write_private(&link, "ours").is_err());
        assert_eq!(
            std::fs::read_to_string(&planted).expect("read what it points at"),
            "attacker-known",
            "the token was written through the link"
        );

        // Ours, but not private.
        let open = dir.join("open.token");
        write_private(&open, "0123456789abcdef").expect("write the token");
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o644)).expect("widen it");
        assert!(
            read_private(&open).is_err(),
            "a readable token must be refused"
        );

        // A directory is not a token either.
        assert!(read_private(&dir).is_err());

        // The one the exclusive create is for: a regular file another
        // account got to the path first, which is neither a symlink nor
        // anything `read_private` was given a chance to see. Writing into it
        // would hand them this session's bearer token, and `mode` would not
        // have applied — it is only consulted for a file being made.
        let raced = dir.join("raced.token");
        std::fs::write(&raced, "").expect("plant the file");
        std::fs::set_permissions(&raced, std::fs::Permissions::from_mode(0o666))
            .expect("as they would leave it");
        assert!(
            write_private(&raced, "0123456789abcdef").is_err(),
            "the token was written into a file anyone can read"
        );
        assert_eq!(
            std::fs::read_to_string(&raced).expect("read it back"),
            "",
            "the token reached it anyway"
        );

        // And the ordinary reason the path is already there: the empty file a
        // previous run left, which is ours and is redrawn over.
        let empty = dir.join("empty.token");
        write_private(&empty, "").expect("leave an empty one");
        write_private(&empty, "0123456789abcdef").expect("redraw over it");
        assert_eq!(
            read_private(&empty).expect("ours reads back").as_deref(),
            Some("0123456789abcdef")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A token this daemon drew is one it accepts, and a remnant is not.
    ///
    /// The truncation case is the one with teeth: a full disk answers
    /// `write_all` with an error after some of the bytes have landed, and
    /// what is left behind is ours by owner and mode, non-empty, and short —
    /// which the old check read as a perfectly good credential. The endpoint
    /// has nothing else to admit on, so a two-character token is a port any
    /// local account can guess its way into.
    #[test]
    fn only_a_whole_token_is_believed() {
        let mut bytes = [0u8; 24];
        getrandom::fill(&mut bytes).expect("randomness");
        let mut drawn = String::new();
        for byte in bytes {
            use std::fmt::Write as _;
            let _ = write!(drawn, "{byte:02x}");
        }
        assert!(is_a_token(&drawn), "a drawn token must read back as one");

        for wrong in [
            "",
            // A prefix, which is what a partial write leaves.
            &drawn[..2],
            &drawn[..TOKEN_CHARS - 1],
            // Longer than one, which is a file somebody appended to.
            &format!("{drawn}0"),
            // The right length, wrong alphabet: uppercase is not what
            // `{:02x}` writes, and neither is anything else.
            &drawn.to_uppercase(),
            &"g".repeat(TOKEN_CHARS),
        ] {
            assert!(!is_a_token(wrong), "{wrong:?} was taken for a token");
        }
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
                !asking(query).token_matches(&config()),
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
                asking(query).token_matches(&config()),
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
        assert!(asking("token=a%20b").token_matches(&config));
    }
}
