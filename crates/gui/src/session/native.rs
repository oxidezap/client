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
use oxidezap_ipc::{Endpoint, Link};

use super::attach;
use super::frames::Frames;
use super::media::Directory;
use super::sink::{Events, ReaderSink};
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
    connect_over(connect_or_start()?)
}

/// Everything after the connection: split it, say hello, and start a reader.
///
/// Taken apart from [`connect`] so an endpoint can come from somewhere other
/// than the daemon's own name — which is what lets a test stand a listener up
/// of its own and watch what dropping the session does to a *real* reader.
/// The alternative is asserting that `Drop` was called, which is the one
/// thing that was never in doubt.
fn connect_over(endpoint: Endpoint) -> std::io::Result<(Session, Events)> {
    let (reader, writer) = endpoint.split()?;
    // Taken before the reader is handed to its thread, because after that
    // there is nothing left to ask.
    let hangup = reader.hangup()?;

    let attach::Attached {
        mut session,
        events,
        sink,
        pending,
        pictures,
    } = attach::begin(
        Link::over_stream(writer),
        Arc::new(Directory),
        // Said rather than left to the default: this is the client the
        // daemon's `ShowWindow` is for, and a front end that stays quiet
        // about it is one the daemon has to guess about.
        true,
    )?;

    // Dropped when the thread ends, whichever way it ends, so the wait below
    // is over the thread's whole life rather than over a message it might
    // not reach.
    let (alive, until_gone) = std::sync::mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("oxidezap-ipc".to_string())
        .spawn(move || {
            let _alive = alive;
            read_frames(reader, &sink, &pending, &pictures);
        })?;

    session.ends_with(Teardown::new(move || {
        hangup.hang_up();
        // Off the dropping thread: waiting here used to block whoever
        // dropped the session — on window close, the UI thread — for up to
        // `READER_PATIENCE` while the window was going away, which on
        // Windows surfaced as the window going invalid under a close it was
        // still tearing down. The wait only reaps the reader, so it keeps a
        // thread of its own the way `reap` keeps one for the child.
        let _ = std::thread::Builder::new()
            .name("oxidezap-ipc-wait".to_string())
            .spawn(move || {
                let _ = until_gone.recv_timeout(READER_PATIENCE);
            });
    }));

    Ok((session, events))
}

/// Read frames until the daemon goes away.
///
/// A loop of its own, and not [`super::attach::read_frames`]: that one is for
/// a reader handed its frames on a task, and this one *is* the read — parked
/// in a blocking call, framing the bytes itself, and wrapped in the
/// `catch_unwind` a thread needs and a task does not. What the two share is
/// [`Frames`], which is the whole of what a frame means.
///
/// Whatever ends the loop, the reporting has to run: draining `pending` and
/// failing every request in it, and telling the window the connection is
/// gone. That block used to sit at the end of the loop and be reached only by
/// a `break`, so a panic anywhere inside unwound straight past it — leaving a
/// window that still reads as connected, with no events arriving, every send
/// spinning on an answer nothing will produce, and no reconnect scheduled.
fn read_frames(
    stream: oxidezap_ipc::Reader,
    events: &ReaderSink,
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
                // Named rather than left to the outage screen: this ending
                // arms no countdown, because the frame that overran is one
                // the daemon rebuilds on every attach and reconnecting meets
                // the same one.
                log::error!(
                    "the daemon sent a frame past {} bytes with no end to it",
                    oxidezap_ipc::MAX_DAEMON_FRAME_BYTES
                );
                frames.fault(oxidezap_core::Fault::oversized(format!(
                    "the background service sent a frame larger than {} bytes",
                    oxidezap_ipc::MAX_DAEMON_FRAME_BYTES
                )));
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
    let program = daemon_program().ok_or_else(|| {
        std::io::Error::other(
            "no daemon beside this binary to start; the two ship in one directory",
        )
    })?;
    let deadline = wacore::time::Instant::now() + START_TIMEOUT;
    // The daemon this call started, kept so it can be asked whether it is
    // still running rather than left to become a zombie.
    let mut started: Option<std::process::Child> = None;

    loop {
        match Endpoint::connect() {
            Ok(stream) => {
                reap(started.take());
                return Ok(stream);
            }
            Err(e) if wacore::time::Instant::now() >= deadline => {
                // The last one this call started, on the way out. Dropping a
                // `Child` waits for nothing: on unix the process stays a
                // zombie until this window exits, and the error screen retries
                // startup, so repeated failures accumulate them.
                reap(started.take());
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
                started = Some(detached_command(&program).spawn().map_err(|e| {
                    std::io::Error::other(format!("could not start {}: {e}", program.display()))
                })?);
            }
        }

        // Polled rather than waited on: the daemon binds after it has taken
        // its lock and prepared its directory, and there is no signal for that
        // short of the socket answering.
        let attempt = wacore::time::Instant::now() + START_ATTEMPT;
        while wacore::time::Instant::now() < attempt {
            if let Ok(stream) = Endpoint::connect() {
                reap(started.take());
                return Ok(stream);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

/// Wait for a daemon this process started, in a thread of its own.
///
/// The connection succeeding is not the end of the child: on Unix a process
/// nobody waits on is a zombie from the moment it exits until its parent
/// does, and the daemon outliving the window is the ordinary case — so the
/// wait belongs somewhere that can outlive the connect loop. One parked
/// thread per daemon this process started, which is at most one.
fn reap(child: Option<std::process::Child>) {
    let Some(mut child) = child else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("oxidezap-daemon-wait".to_string())
        .spawn(move || {
            let _ = child.wait();
        });
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

/// How the daemon is launched: detached, on every platform.
///
/// The mirror of the daemon's own `detached_command` for the other
/// direction — same name and same split, one per binary rather than one
/// shared, because the two launches differ in what they inherit: the
/// daemon's log belongs in the daemon's, not interleaved into this window's,
/// while a front end started from a terminal keeps the terminal's.
#[cfg(windows)]
fn detached_command(program: &std::path::Path) -> std::process::Command {
    use std::os::windows::process::CommandExt as _;

    // A console-subsystem child pops a console window of its own unless told
    // not to: opening the GUI popped a terminal with the daemon in it. A
    // GUI-subsystem parent still spawns a console child visibly, so the flag
    // is needed regardless of our own subsystem.
    let mut command = std::process::Command::new(program);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    command
}

/// Everywhere else a spawn is an ordinary spawn with quiet stdio.
#[cfg(not(windows))]
fn detached_command(program: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
}

/// What dropping a connection does to the daemon at the other end.
///
/// Unix, and that is a statement about this box rather than about the code:
/// the endpoint's platform split is `oxidezap_ipc`'s, a listener of our own is
/// a `UnixListener` here and a named pipe there, and CI's `cross` job is what
/// runs the pipe. What these prove is the half that is written *here* — that
/// the owner ends the connection, that a handle does not, and in which order
/// the two halves of the ending happen — and that half is one program on
/// either platform.
#[cfg(all(test, unix))]
mod tests {
    use std::io::{BufRead as _, BufReader};
    use std::sync::Arc;
    use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};

    use oxidezap_ipc::{ClientRequest, Endpoint, Link};

    use super::Teardown;
    use super::{Session, connect_over};

    /// Long enough for a local socket to carry a line and a close, short
    /// enough that a test that is going to fail does it now.
    const SOON: std::time::Duration = std::time::Duration::from_secs(5);

    /// Long enough that a connection which was going to end would have.
    ///
    /// Only ever used to prove a *negative*, so it is a floor on the waiting
    /// rather than a deadline: a slow box makes this test slower and never
    /// makes it wrong.
    const A_MOMENT: std::time::Duration = std::time::Duration::from_millis(300);

    /// A daemon, to the extent these tests need one: something that accepts a
    /// connection and says what it reads.
    struct Peer {
        /// One item per frame the client sent, and `None` when the client's
        /// end of the connection closed.
        heard: Receiver<Option<String>>,
        path: std::path::PathBuf,
    }

    impl Peer {
        /// Listen, accept one client, and report every line until it ends.
        fn listening(name: &str) -> std::io::Result<Self> {
            let path = std::env::temp_dir().join(format!(
                "oxidezap-session-{}-{name}.sock",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&path);
            let listener = std::os::unix::net::UnixListener::bind(&path)?;
            let (told, heard) = channel();
            std::thread::Builder::new()
                .name(format!("peer-{name}"))
                .spawn(move || {
                    let Ok((stream, _)) = listener.accept() else {
                        return;
                    };
                    let mut lines = BufReader::new(stream).lines();
                    while let Some(Ok(line)) = lines.next() {
                        if told.send(Some(line)).is_err() {
                            return;
                        }
                    }
                    // Read returned nothing: the client's write half is gone,
                    // which is the ending this whole exercise is about.
                    let _ = told.send(None);
                })?;
            Ok(Self { heard, path })
        }

        /// The client this peer is listening for.
        fn client(&self) -> std::io::Result<(Session, super::Events)> {
            connect_over(Endpoint::connect_at(&self.path)?)
        }

        /// The next frame, which every connection begins with: the hello.
        fn hello(&self) {
            assert!(
                matches!(self.heard.recv_timeout(SOON), Ok(Some(line)) if line.contains("\"request\":\"hello\"")),
                "a connection says hello before anything else"
            );
        }

        /// Whether the connection ended, waiting for it if it has not.
        fn ended(&self) -> bool {
            loop {
                match self.heard.recv_timeout(SOON) {
                    // A frame in flight is not the ending; keep reading.
                    Ok(Some(_)) => continue,
                    Ok(None) | Err(RecvTimeoutError::Disconnected) => return true,
                    Err(RecvTimeoutError::Timeout) => return false,
                }
            }
        }

        /// Whether the connection is still up a moment later, with nothing
        /// having been said on it.
        ///
        /// Both halves, because both are assertions the callers want: an
        /// ending would arrive as `None`, and a frame that should never have
        /// been written would arrive as a line. Answering `true` to a stray
        /// frame would make this the assertion that cannot fail.
        fn quiet(&self) -> bool {
            matches!(
                self.heard.recv_timeout(A_MOMENT),
                Err(RecvTimeoutError::Timeout)
            )
        }
    }

    impl Drop for Peer {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// The owner is what ends the connection, and it ends it for good.
    ///
    /// The reader is a thread parked in a blocking read that nothing wakes,
    /// so this is not a fact about `Drop` running — it is a fact about the
    /// daemon at the other end seeing the connection close, which is the only
    /// thing that stops it counting a client. Asserted on the peer, because
    /// asserting that a destructor ran would prove the part nobody doubted.
    #[test]
    fn dropping_the_session_ends_the_connection() {
        let peer = Peer::listening("dropped").expect("a listener of our own");
        let (session, _events) = peer.client().expect("connect to it");
        peer.hello();

        // Held across the drop on purpose: a part of the window that kept a
        // handle must not keep the connection.
        let stale = session.handle();
        drop(session);

        assert!(peer.ended(), "the daemon's end of the connection closed");
        assert!(
            stale.send(ClientRequest::ReloadHistory).is_err(),
            "and a handle that outlived it sends nowhere"
        );
    }

    /// A handle is a refcount, not an owner: dropping one ends nothing.
    #[test]
    fn dropping_a_handle_leaves_the_connection_alone() {
        let peer = Peer::listening("handle").expect("a listener of our own");
        let (session, _events) = peer.client().expect("connect to it");
        peer.hello();

        drop(session.handle());

        assert!(
            peer.quiet(),
            "the connection outlives a handle, and nothing was said on it"
        );
        session
            .send(ClientRequest::ReloadHistory)
            .expect("and still sends");
        assert!(
            matches!(peer.heard.recv_timeout(SOON), Ok(Some(_))),
            "the request arrived"
        );
    }

    /// The write half is given up before the hangup runs.
    ///
    /// The order matters on a named pipe, where cancelling the read
    /// disconnects nothing — the pipe breaks when the last handle to it
    /// closes — and it cannot be observed on a Unix socket, whose shutdown
    /// ends the read either way. So it is observed from inside the teardown
    /// instead: the closure that would hang up asks a handle to send, and
    /// what it gets back says whether the link was already gone. That
    /// question has one answer on both platforms.
    #[test]
    fn the_link_is_released_before_the_hangup_runs() {
        let peer = Peer::listening("order").expect("a listener of our own");
        let (_reader, writer) = Endpoint::connect_at(&peer.path)
            .expect("connect to it")
            .split()
            .expect("two halves");
        let attached =
            super::attach::begin(Link::over_stream(writer), Arc::new(super::Directory), true)
                .expect("say hello");
        peer.hello();

        let mut session = attached.session;
        let handle = session.handle();
        let (told, what_the_hangup_saw) = channel();
        session.ends_with(Teardown::new(move || {
            told.send(handle.send(ClientRequest::ReloadHistory).is_err())
                .expect("the test is still standing here");
        }));

        drop(session);
        assert_eq!(
            what_the_hangup_saw.recv_timeout(SOON),
            Ok(true),
            "the link was already given up when the hangup ran"
        );
        assert!(
            peer.quiet(),
            "and the request the hangup tried to make never reached the daemon"
        );
    }
}
