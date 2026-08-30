//! The daemon, from a process that can open a socket.
//!
//! A thread parks in a blocking read for as long as the connection lives, and
//! the rest of the program writes while it does. That is all a
//! newline-delimited protocol over a local transport needs, and it is why
//! nothing here is async: the runtime the old client owned existed for the
//! network, and the network is somebody else's problem now.
//!
//! Starting a daemon is also this side's job, and only this side's: a page
//! cannot spawn a process, which is why the two front ends differ about what
//! "no daemon" means. Here it means "start one"; there it means "say so".

use std::io::BufReader;
use std::sync::Arc;

use log::info;
use oxidezap_ipc::{ClientRequest, Endpoint, Link, PROTOCOL_VERSION};

use super::frames::Frames;
use super::media::Directory;
use super::sink::{self, EventSink, Events};
use super::{Pending, Session, Teardown};

/// How long to give the reader to notice it has been hung up on.
///
/// It normally leaves at once — the connection is shut down before this
/// waits, so its read returns end of file. The bound is for the one case that
/// is not instant: the reader parked publishing into a queue that only the UI
/// drains, which is the thread standing here. Waiting for it forever would
/// deadlock the reconnect; waiting a moment and moving on costs a thread that
/// is already on its way out.
const READER_PATIENCE: std::time::Duration = std::time::Duration::from_secs(2);

/// Connect to the daemon, starting one if nothing is listening.
///
/// Returns the events it will publish. The daemon reloads history for a
/// client that asks for events, so the chats arrive without being asked
/// for separately.
pub(super) fn connect() -> std::io::Result<(Session, Events)> {
    let (reader, writer) = connect_or_start()?.split()?;
    // Taken before the reader is handed to its thread, because after that
    // there is nothing left to ask.
    let hangup = reader.hangup()?;
    let (events, rx) = sink::channel();

    let mut session = Session::new(
        Link::over_stream(writer),
        events.clone(),
        Arc::new(Directory),
    );

    // Before the reader starts, because the daemon serves nothing until it
    // has one and answers it with the history this connection asked for.
    session.send(ClientRequest::Hello {
        protocol: PROTOCOL_VERSION,
        session_events: true,
        // Said rather than left to the default: this is the client the
        // daemon's `ShowWindow` is for, and a front end that stays quiet
        // about it is one the daemon has to guess about.
        has_window: true,
    })?;

    let pending = Arc::clone(&session.pending);
    let pictures = session.call_frames().clone();
    // Dropped when the thread ends, whichever way it ends, so the wait below
    // is over the thread's whole life rather than over a message it might
    // not reach.
    let (alive, until_gone) = std::sync::mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("oxidezap-ipc".to_string())
        .spawn(move || {
            let _alive = alive;
            read_frames(reader, &events, &pending, &pictures);
        })?;

    session.ends_with(Teardown::new(move || {
        hangup.hang_up();
        let _ = until_gone.recv_timeout(READER_PATIENCE);
    }));

    Ok((session, rx))
}

/// Read frames until the daemon goes away.
///
/// Whatever ends the loop, the reporting has to run: draining `pending` and
/// failing every request in it, and telling the window the connection is
/// gone. That block used to sit at the end of the loop and be reached only by
/// a `break`, so a panic anywhere inside unwound straight past it — leaving a
/// window that still reads as connected, with no events arriving, every send
/// spinning on an answer nothing will produce, and no reconnect scheduled.
fn read_frames(
    stream: oxidezap_ipc::Reader,
    events: &EventSink,
    pending: &Pending,
    pictures: &crate::video::LatestFrames,
) {
    let cache = Directory;
    let mut frames = Frames::new(events, pending, &cache, pictures);
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        read_loop(stream, &mut frames);
    }))
    .is_err();
    if panicked {
        log::error!("the thread reading the daemon connection panicked");
    }
    frames.finish();
}

fn read_loop(stream: oxidezap_ipc::Reader, frames: &mut Frames<'_>) {
    let mut reader = BufReader::new(stream);
    // Bounded, through the same framing the daemon's own reader is bounded
    // by: reading a frame into a `String` with nothing stopping it means a
    // peer that never sends a newline grows this thread until the window is
    // killed.
    let mut buf = Vec::new();
    loop {
        let frame =
            oxidezap_ipc::read_frame(&mut reader, &mut buf, oxidezap_ipc::MAX_DAEMON_FRAME_BYTES);
        let line = match frame {
            Ok(Some(oxidezap_ipc::FrameRead::Line(line))) => line,
            Ok(Some(oxidezap_ipc::FrameRead::NotUtf8)) => {
                log::warn!("the daemon sent a frame that is not text; ignoring it");
                continue;
            }
            Ok(Some(oxidezap_ipc::FrameRead::TooLong)) => {
                log::error!(
                    "the daemon sent a frame past {} bytes with no end to it",
                    oxidezap_ipc::MAX_DAEMON_FRAME_BYTES
                );
                break;
            }
            Ok(None) => break,
            Err(e) => {
                log::error!("lost the daemon connection: {e}");
                break;
            }
        };

        if let Some(message) = super::frames::parse(&line)
            && frames.apply(message).is_break()
        {
            break;
        }
    }
}

/// How long to keep trying before giving the user an error instead.
const START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long to leave a daemon we started to take its lock and bind.
const START_ATTEMPT: std::time::Duration = std::time::Duration::from_secs(2);

/// Connect, starting `oxidezapd` for as long as nothing is listening.
///
/// The front end no longer owns a session, so there has to be one: a first run
/// on a fresh machine would otherwise show an error where it should show a QR
/// code.
///
/// Starting one is safe to race, because the daemon takes a per-user lock and
/// the loser exits. That is also why one attempt is not enough. A daemon
/// started while another is still tearing down loses the lock and exits, and
/// the socket was unlinked before that lock was released — so there is a
/// window where nothing is listening, nothing is starting, and a single-shot
/// spawn has already given up. Retrying until the deadline is what closes it,
/// and it is why nothing here watches the socket to decide the old daemon has
/// gone: the socket goes first.
fn connect_or_start() -> std::io::Result<Endpoint> {
    // Only for the message: connecting is the endpoint's business, and on
    // Windows this is a pipe name rather than anything on disk.
    let path = oxidezap_ipc::endpoint_path()
        .ok_or_else(|| std::io::Error::other("no per-user directory to look for the daemon in"))?;
    let program = daemon_program();
    let deadline = wacore::time::Instant::now() + START_TIMEOUT;
    // The daemon this call started, kept so it can be asked whether it is
    // still running rather than left to become a zombie.
    let mut started: Option<std::process::Child> = None;

    loop {
        match Endpoint::connect() {
            Ok(stream) => return Ok(stream),
            Err(e) if wacore::time::Instant::now() >= deadline => {
                return Err(std::io::Error::other(format!(
                    "no daemon listening on {} after {START_TIMEOUT:?}: {e}",
                    path.display()
                )));
            }
            Err(_) => {}
        }

        // Only if the last one is not still coming up. The connect above can
        // fail for reasons that are not "nobody is listening" — a socket this
        // user may not open, a peer that is not us — and each turn of the
        // loop launching another daemon meant five of them per attempt, all
        // but one losing the per-user lock and exiting. Held rather than
        // dropped, and reaped: a `Child` nobody waits on is a zombie for as
        // long as this window lives, and the error screen retries every
        // fifteen seconds for as long as the user leaves it up.
        match started.as_mut().map(std::process::Child::try_wait) {
            Some(Ok(None)) => {}
            _ => {
                info!("no daemon on {}; starting one", path.display());
                started = Some(
                    std::process::Command::new(&program)
                        // The daemon's own log belongs in the daemon's, not
                        // interleaved into this window's.
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                        .map_err(|e| {
                            std::io::Error::other(format!(
                                "could not start {}: {e}",
                                program.display()
                            ))
                        })?,
                );
            }
        }

        // Polled rather than waited on: the daemon binds after it has taken
        // its lock and prepared its directory, and there is no signal for that
        // short of the socket answering.
        let attempt = wacore::time::Instant::now() + START_ATTEMPT;
        while wacore::time::Instant::now() < attempt {
            if let Ok(stream) = Endpoint::connect() {
                return Ok(stream);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

/// Where to find the daemon.
///
/// Beside this binary first: the two ship together and a release directory is
/// not on anybody's `PATH`. A bare name otherwise, so a development build run
/// from `cargo` finds the one on the path.
fn daemon_program() -> std::path::PathBuf {
    const NAME: &str = if cfg!(windows) {
        "oxidezapd.exe"
    } else {
        "oxidezapd"
    };
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(NAME)))
        .filter(|path| path.exists())
        .unwrap_or_else(|| std::path::PathBuf::from(NAME))
}
