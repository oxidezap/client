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

use std::io::{BufRead, BufReader};
use std::sync::Arc;

use log::info;
use oxidezap_ipc::{ClientRequest, Endpoint, Link, PROTOCOL_VERSION};

use super::frames::Frames;
use super::media::Directory;
use super::sink::{self, EventSink, Events};
use super::{Pending, Session};

/// Connect to the daemon, starting one if nothing is listening.
///
/// Returns the events it will publish. The daemon reloads history for a
/// client that asks for events, so the chats arrive without being asked
/// for separately.
pub(super) fn connect() -> std::io::Result<(Session, Events)> {
    let (reader, writer) = connect_or_start()?.split()?;
    let (events, rx) = sink::channel();

    let session = Session::new(
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
    std::thread::Builder::new()
        .name("oxidezap-ipc".to_string())
        .spawn(move || read_frames(reader, &events, &pending, &pictures))?;

    Ok((session, rx))
}

/// Read frames until the daemon goes away.
fn read_frames(
    stream: oxidezap_ipc::Reader,
    events: &EventSink,
    pending: &Pending,
    pictures: &crate::video::LatestFrames,
) {
    let cache = Directory;
    let mut frames = Frames::new(events, pending, &cache, pictures);
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                log::error!("lost the daemon connection: {e}");
                break;
            }
        }

        if let Some(message) = super::frames::parse(&line)
            && frames.apply(message).is_break()
        {
            break;
        }
    }
    frames.finish();
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
    let program = daemon_program().ok_or_else(|| {
        std::io::Error::other(
            "no daemon beside this binary to start; the two ship in one directory",
        )
    })?;
    let deadline = wacore::time::Instant::now() + START_TIMEOUT;

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

        info!("no daemon on {}; starting one", path.display());
        std::process::Command::new(&program).spawn().map_err(|e| {
            std::io::Error::other(format!("could not start {}: {e}", program.display()))
        })?;

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
/// Beside this binary, and nowhere else: the two ship together and a release
/// directory is not on anybody's `PATH`.
///
/// `None` rather than a bare name, which `PATH` would resolve — the same
/// reason the daemon's `front_end_program` gives for the other direction: a
/// window started from an arranged environment would start whatever that
/// environment calls `oxidezapd`, and there is no session more sensitive
/// than the one that holds the account.
fn daemon_program() -> Option<std::path::PathBuf> {
    const NAME: &str = if cfg!(windows) {
        "oxidezapd.exe"
    } else {
        "oxidezapd"
    };
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(NAME)))
        .filter(|path| path.exists())
}
