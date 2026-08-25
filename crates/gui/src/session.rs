//! The daemon, from this side of the socket.
//!
//! Deliberately shaped like the `WhatsAppClient` it replaces: same method
//! names, same arguments, same fire-and-forget for everything whose outcome
//! arrives as an event. The front end above it does not know which side of a
//! socket the session is on, which is why moving it there changed almost
//! nothing in the app.
//!
//! No async runtime. One thread reads frames and one mutex serializes writes,
//! which is all a newline-delimited protocol over a local socket needs; the
//! runtime the old client owned existed for the network, and the network is
//! now somebody else's problem.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use log::{debug, error, info, warn};
use oxidezap_core::{DownloadableMedia, MediaContent, UiEvent};
use oxidezap_ipc::{
    CallAction, ClientRequest, DaemonMessage, PROTOCOL_VERSION, RequestId, media_path, socket_path,
};
use portable_atomic::AtomicU64;
use tokio::sync::{mpsc, oneshot};

/// Answers still owed to a caller, by the id they were asked under.
type Pending = Arc<Mutex<HashMap<RequestId, oneshot::Sender<Result<Vec<u8>, String>>>>>;

/// The write half, shared with the reader thread.
///
/// Shared because recovery is something the reader decides: it is the side
/// that sees a `Resync`, and answering one means sending a request.
type Writer = Arc<Mutex<UnixStream>>;

/// A connection to `oxidezapd`.
pub struct Session {
    writer: Writer,
    pending: Pending,
    next_id: AtomicU64,
}

impl Session {
    /// Connect to the daemon, starting one if nothing is listening.
    ///
    /// Returns the events it will publish. The daemon reloads history for a
    /// client that asks for events, so the chats arrive without being asked
    /// for separately.
    pub fn connect() -> std::io::Result<(Self, mpsc::UnboundedReceiver<UiEvent>)> {
        let stream = connect_or_start()?;
        let reader = stream.try_clone()?;

        let session = Self {
            writer: Arc::new(Mutex::new(stream)),
            pending: Pending::default(),
            next_id: AtomicU64::new(1),
        };
        // Before the reader starts, because the daemon serves nothing until it
        // has one and answers it with the history this connection asked for.
        session.send(&ClientRequest::Hello {
            protocol: PROTOCOL_VERSION,
            session_events: true,
        })?;

        let (events, rx) = mpsc::unbounded_channel();
        let pending = Arc::clone(&session.pending);
        let writer = Arc::clone(&session.writer);
        std::thread::Builder::new()
            .name("oxidezap-ipc".to_string())
            .spawn(move || read_frames(reader, &events, &pending, &writer))?;

        Ok((session, rx))
    }

    fn send(&self, request: &ClientRequest) -> std::io::Result<()> {
        write_request(&self.writer, request)
    }

    /// Send and log rather than propagate.
    ///
    /// Every caller here is a UI action whose outcome arrives as an event, and
    /// a socket that has broken takes the whole session with it — there is no
    /// per-action recovery for the caller to perform.
    fn tell(&self, request: &ClientRequest) {
        if let Err(e) = self.send(request) {
            error!("could not reach the daemon: {e}");
        }
    }

    pub fn send_message(&self, jid: &str, text: &str, local_id: String) {
        self.tell(&ClientRequest::SendText {
            jid: jid.to_string(),
            text: text.to_string(),
            local_id: Some(local_id),
        });
    }

    pub fn send_audio_message(
        &self,
        jid: &str,
        audio: Vec<u8>,
        duration_secs: u32,
        waveform: Vec<u8>,
        local_id: String,
    ) {
        // Through the media cache: a voice note is the one thing this side
        // sends that does not belong in a frame. The key is the local id,
        // which is already unique per recording.
        let upload = format!("u-{}", sanitize(&local_id));
        let Some(path) = media_path(&upload) else {
            error!("no media cache to stage the recording in");
            return;
        };
        if let Some(dir) = path.parent()
            && let Err(e) =
                std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&path, &audio))
        {
            error!("could not stage the recording: {e}");
            return;
        }
        self.tell(&ClientRequest::SendAudio {
            jid: jid.to_string(),
            upload,
            duration_secs,
            waveform,
            local_id: Some(local_id),
        });
    }

    pub fn send_composing(&self, jid: &str) {
        self.typing(jid, true);
    }

    pub fn send_paused(&self, jid: &str) {
        self.typing(jid, false);
    }

    fn typing(&self, jid: &str, composing: bool) {
        self.tell(&ClientRequest::Typing {
            jid: jid.to_string(),
            composing,
        });
    }

    /// Mark a chat read up to the message the UI is looking at.
    ///
    /// One request where the old client took two: the daemon owns the read
    /// boundary and the receipts that go with it, so a front end no longer
    /// computes either. `through_message_id` is the newest message this side
    /// holds, and the daemon refuses anything else — a read is irreversible
    /// and must not reach past what the user has seen.
    pub fn mark_chat_read(&self, jid: &str, through_message_id: Option<String>) {
        self.tell(&ClientRequest::MarkRead {
            jid: jid.to_string(),
            through_message_id,
        });
    }

    pub fn start_call(&self, jid: &str, is_video: bool, placeholder_id: String) {
        self.call(CallAction::Start {
            jid: jid.to_string(),
            video: is_video,
            placeholder_id,
        });
    }

    pub fn accept_call(&self, call_id: &str) {
        self.call(CallAction::Accept {
            call_id: call_id.to_string(),
        });
    }

    pub fn decline_call(&self, call_id: &str) {
        self.call(CallAction::Decline {
            call_id: call_id.to_string(),
        });
    }

    pub fn cancel_call(&self, call_id: &str) {
        self.call(CallAction::Cancel {
            call_id: call_id.to_string(),
        });
    }

    pub fn set_call_muted(&self, call_id: &str, muted: bool) {
        self.call(CallAction::SetMuted {
            call_id: call_id.to_string(),
            muted,
        });
    }

    fn call(&self, action: CallAction) {
        self.tell(&ClientRequest::Call(action));
    }

    /// Wipe the local store and pair again.
    ///
    /// The daemon owns that file and stops itself once it is gone, so this is
    /// the last thing this connection will be told anything on.
    pub fn forget_session(&self) {
        self.tell(&ClientRequest::ForgetSession);
    }

    /// Wait for a daemon we have asked to stop to actually be gone.
    ///
    /// Reconnecting straight away lands on the socket of the process that is
    /// still tearing down — it has a session to disconnect and a database to
    /// close — and that connection dies with it. Blocking, so call it off the
    /// UI thread; bounded, so a daemon that will not die leaves the front end
    /// with an error rather than a spinner.
    pub fn wait_for_shutdown() {
        let Some(path) = socket_path() else { return };
        let deadline = wacore::time::Instant::now() + START_TIMEOUT;
        while UnixStream::connect(&path).is_ok() {
            if wacore::time::Instant::now() >= deadline {
                warn!("the old daemon is still listening on {}", path.display());
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Fetch media, answered when the bytes are on disk.
    ///
    /// The same signature the old client had, so the callers that thread this
    /// receiver through a timeout did not change. The bytes come back rather
    /// than a path because that is what every caller wanted anyway.
    pub fn download_downloadable_media(
        &self,
        media: DownloadableMedia,
    ) -> oneshot::Receiver<Result<Vec<u8>, String>> {
        let (tx, rx) = oneshot::channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);

        if let Err(e) = self.send(&ClientRequest::Download {
            id,
            media: Box::new(media),
        }) {
            // Answer here rather than leaving the caller waiting on a request
            // that never went out.
            if let Some(tx) = self
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id)
            {
                let _ = tx.send(Err(format!("could not reach the daemon: {e}")));
            }
        }
        rx
    }
}

/// Keep a key to what [`media_path`] accepts.
fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '.'
            }
        })
        .take(120)
        .collect()
}

fn write_request(writer: &Writer, request: &ClientRequest) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(request).map_err(std::io::Error::other)?;
    line.push(b'\n');
    writer
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .write_all(&line)
}

/// Read frames until the daemon goes away.
fn read_frames(
    stream: UnixStream,
    events: &mpsc::UnboundedSender<UiEvent>,
    pending: &Pending,
    writer: &Writer,
) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                error!("lost the daemon connection: {e}");
                break;
            }
        }

        match serde_json::from_str::<DaemonMessage>(line.trim_end()) {
            Ok(DaemonMessage::Session { event }) => {
                let mut event = *event;
                // On this thread rather than the UI's: a history load names
                // every photo in the account, and reading them is I/O.
                load_media(&mut event);
                if events.send(event).is_err() {
                    break;
                }
            }
            Ok(DaemonMessage::Downloaded { id, result }) => {
                let Some(tx) = pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id)
                else {
                    debug!("a download answer arrived for {id}, which nobody is waiting on");
                    continue;
                };
                let _ = tx.send(result.and_then(|key| {
                    media_path(&key)
                        .ok_or_else(|| format!("the daemon named an unusable cache key: {key}"))
                        .and_then(|path| std::fs::read(path).map_err(|e| e.to_string()))
                }));
            }
            // The daemon truncated our stream. Nothing here can patch the
            // gap — this side holds messages, not summaries — so it starts
            // over, which is what attaching does anyway. Asked from this
            // thread because this is the thread that finds out.
            Ok(DaemonMessage::Resync) => {
                warn!("fell behind the daemon; asking for the history again");
                if let Err(e) = write_request(writer, &ClientRequest::ReloadHistory) {
                    error!("could not ask the daemon to resend the history: {e}");
                    break;
                }
            }
            Ok(DaemonMessage::Error(e)) => error!("daemon refused a request: {e}"),
            // Summaries, acknowledgements and window requests: the daemon
            // serves other front ends too, and this one derives its own state
            // from the session stream.
            Ok(_) => {}
            Err(e) => error!("unparsable frame from the daemon: {e}"),
        }
    }
    info!("daemon connection closed");
}

/// Fill in the media bytes the daemon left in its cache.
///
/// The inverse of what the daemon does on the way out. Media is the one thing
/// the wire does not carry, so this is the one place the front end has to know
/// that — every renderer above it still just reads `data`.
fn load_media(event: &mut UiEvent) {
    match event {
        UiEvent::MessageReceived { message, .. } => fill(&mut message.media),
        UiEvent::HistoryLoaded { chats, .. } => {
            for chat in chats {
                for message in &mut chat.messages {
                    fill(&mut message.media);
                }
            }
        }
        _ => {}
    }
}

fn fill(media: &mut Option<MediaContent>) {
    let Some(media) = media else { return };
    let Some(key) = media.cache_key.take() else {
        return;
    };
    match media_path(&key).map(std::fs::read) {
        Some(Ok(bytes)) => media.data = Arc::new(bytes),
        // The renderer falls back to offering the download, which is the same
        // thing it does for media that was never cached.
        Some(Err(e)) => debug!("media {key} is not in the cache: {e}"),
        None => warn!("the daemon named an unusable cache key: {key}"),
    }
}

/// How long to keep trying a daemon we have just started.
const START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Connect, starting `oxidezapd` if nothing is listening yet.
///
/// The front end no longer owns a session, so there has to be one: a first run
/// on a fresh machine would otherwise show an error where it should show a QR
/// code. Starting it is safe to race — the daemon takes a per-user lock and a
/// second one exits — so two front ends opening at once end up on the same
/// session rather than fighting over the store.
fn connect_or_start() -> std::io::Result<UnixStream> {
    let path = socket_path().ok_or_else(|| {
        std::io::Error::other("no runtime directory to look for the daemon's socket in")
    })?;
    if let Ok(stream) = UnixStream::connect(&path) {
        return Ok(stream);
    }

    info!("no daemon listening on {}; starting one", path.display());
    let program = daemon_program();
    std::process::Command::new(&program).spawn().map_err(|e| {
        std::io::Error::other(format!("could not start {}: {e}", program.display()))
    })?;

    // Polled rather than waited on: the daemon binds after it has taken its
    // lock and prepared its directory, and there is no signal for that short
    // of the socket appearing.
    let deadline = wacore::time::Instant::now() + START_TIMEOUT;
    loop {
        match UnixStream::connect(&path) {
            Ok(stream) => return Ok(stream),
            Err(e) if wacore::time::Instant::now() >= deadline => {
                return Err(std::io::Error::other(format!(
                    "started {} but it never listened on {}: {e}",
                    program.display(),
                    path.display()
                )));
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
}

/// Where to find the daemon.
///
/// Beside this binary first: the two ship together and a release directory is
/// not on anybody's `PATH`. A bare name otherwise, so a development build run
/// from `cargo` finds the one on the path.
fn daemon_program() -> std::path::PathBuf {
    const NAME: &str = "oxidezapd";
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(NAME)))
        .filter(|path| path.exists())
        .unwrap_or_else(|| std::path::PathBuf::from(NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recording's key is a local id, which the front end composes; it
    /// still has to be a plain file name.
    #[test]
    fn a_staged_recording_cannot_escape_the_cache() {
        for id in ["../../etc/passwd", "local/1", "local 1"] {
            let key = format!("u-{}", sanitize(id));
            assert!(media_path(&key).is_some(), "{id} produced {key}");
            assert!(!key.contains('/'), "{key}");
        }
    }

    /// The bytes are the one thing the wire does not carry, so a message
    /// arrives naming its media rather than holding it.
    #[test]
    fn media_bytes_do_not_survive_a_round_trip_through_a_frame() {
        let media = MediaContent {
            media_type: oxidezap_core::MediaType::Image,
            data: Arc::new(vec![7; 4096]),
            cache_key: Some("m-abc".into()),
            mime_type: "image/jpeg".into(),
            width: None,
            height: None,
            caption: None,
            file_name: None,
            downloadable: None,
            is_animated: false,
            duration_secs: None,
            data_is_preview: false,
        };
        let small = serde_json::to_string(&media).unwrap();
        let bigger = serde_json::to_string(&MediaContent {
            data: Arc::new(vec![7; 1024 * 1024]),
            ..media
        })
        .unwrap();
        assert_eq!(
            small.len(),
            bigger.len(),
            "a frame's size must not depend on how big the photo in it is"
        );

        let back: MediaContent = serde_json::from_str(&small).unwrap();
        assert!(back.data.is_empty(), "and the bytes do not come back");
        assert_eq!(back.cache_key.as_deref(), Some("m-abc"));
    }
}
