//! The daemon over a byte stream: a Unix socket, or a Windows named pipe.
//!
//! Both are things only a process on this machine can open, which is why they
//! were chosen — the endpoint carries control of a WhatsApp session. Both are
//! read and written with `std::io`, so everything above this — the framing,
//! the requests, the whole protocol — is the same code on either, and a front
//! end never mentions which.
//!
//! Blocking, and deliberately: a newline-delimited protocol over a local
//! transport needs one thread to read and a lock to serialize writes, not a
//! runtime. The daemon is the side that has thousands of things happening at
//! once; a client has one.
//!
//! Those two threads run *at the same time*, which is a requirement on the
//! transport rather than a detail of the caller — [`Endpoint::split`] is where
//! it is written down, because a platform can quietly fail to provide it. See
//! the `overlapped` module for the one that did.

use std::io::{Read, Write};

#[cfg(windows)]
mod overlapped;

/// A connection to the daemon, before it is put to work.
///
/// Not readable or writable itself: it becomes a [`Reader`] and a [`Writer`],
/// and the point of making that a step is that the two are used from different
/// threads at once. A transport that cannot do that is broken for this
/// protocol, and there is now one place that says so.
pub struct Endpoint(Inner);

/// The half a reader thread parks in.
pub struct Reader(Inner);

/// The half everything else writes through, behind a lock.
pub struct Writer(Inner);

#[cfg(unix)]
type Inner = std::os::unix::net::UnixStream;

/// A named pipe, opened overlapped.
///
/// Windows has no `std` named-pipe client, and a pipe *is* openable by name
/// with the ordinary file API — but that gives a synchronous handle, on which
/// Windows serializes reads against writes and deadlocks this protocol. See
/// [`overlapped`].
#[cfg(windows)]
type Inner = overlapped::Overlapped;

/// How long one send may sit in a kernel buffer nobody is draining.
///
/// Generous, because the daemon legitimately pauses: a long store read
/// between two frame reads is ordinary and a request that waited a moment is
/// not a broken connection. Past this it is not a pause, and hanging the
/// window on it is the worse of the two answers.
#[cfg(unix)]
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

impl Endpoint {
    /// Connect to the daemon, or report why not.
    ///
    /// `ErrorKind::NotFound` and `ErrorKind::ConnectionRefused` both mean
    /// "nothing is listening" — the first because a Unix socket is a
    /// filesystem entry that may not exist, the second because it may exist
    /// and be stale. A caller deciding whether to start a daemon should treat
    /// them alike.
    pub fn connect() -> std::io::Result<Self> {
        let path = crate::endpoint_path().ok_or_else(|| {
            std::io::Error::other("no per-user directory to look for the daemon in")
        })?;
        Self::connect_at(&path)
    }

    /// Connect to an endpoint by name.
    ///
    /// [`connect`](Self::connect) is this with the daemon's own name. Taking
    /// one is what lets a test stand up a real endpoint of its own — which
    /// matters more than it sounds, because the difference between the two
    /// platforms here is a runtime one that no amount of compiling catches.
    pub fn connect_at(path: &std::path::Path) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            let stream = Inner::connect(path)?;
            check_peer(&stream)?;
            Ok(Self(stream))
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;

            // Read and write, because the pipe is duplex and opening it for
            // one direction would half-connect. Overlapped, because the two
            // directions are used at once and a synchronous handle will not
            // have that — see `overlapped`.
            let pipe = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(overlapped::FILE_FLAG_OVERLAPPED)
                .open(path)?;
            check_peer(&pipe)?;
            overlapped::Overlapped::new(pipe).map(Self)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            Err(std::io::Error::other("no local transport on this platform"))
        }
    }

    /// The two ends, for the two threads that use them.
    ///
    /// One thread parks in a read for as long as the connection lives, and the
    /// rest of the program writes while it does. Both of those are true at the
    /// same time — which reads like a caller's business and is not: on Windows
    /// a synchronous handle makes the write wait for the read, so a request
    /// waits for an answer to the request. Splitting here is what gives that
    /// requirement somewhere to be stated and checked.
    pub fn split(self) -> std::io::Result<(Reader, Writer)> {
        let writer = self.0.try_clone()?;
        // The write half gets a deadline, and only the write half. Sends go
        // out on the caller's own thread — a click handler, a per-frame
        // typing indicator — and a daemon that stops draining this socket
        // fills the kernel's buffer, after which `write_all` blocks with
        // nothing to end it: the window freezes whole, and the lock around
        // the writer freezes every other send with it. A timed-out write
        // surfaces as the same I/O error a broken connection already
        // produces, which every caller here can already report.
        #[cfg(unix)]
        writer.set_write_timeout(Some(WRITE_TIMEOUT))?;
        Ok((Reader(self.0), Writer(writer)))
    }
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// Refuse a daemon that is not us, on the other platform.
///
/// The same sentence as the Unix check above and for the same reason: the
/// name is predictable — `\\.\pipe\oxidezap-<SID>` — and it is not reserved,
/// so another local account can create it before the daemon does. The
/// `first_pipe_instance` flag on the listener protects the daemon *after* it
/// exists, which is the wrong half: the daemon then refuses to start and the
/// client connects to whoever got there first, handing over its session
/// requests and its message text. The kernel knows who is on the other end
/// here too, so ask it.
#[cfg(windows)]
fn check_peer(pipe: &std::fs::File) -> std::io::Result<()> {
    let server = server_sid(pipe)?;
    let us = crate::windows_user::sid_string()?;
    if server == us {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("the daemon pipe is served by {server}, not by us ({us}); refusing to talk to it"),
    ))
}

/// The SID of whoever is serving this pipe.
#[cfg(windows)]
fn server_sid(pipe: &std::fs::File) -> std::io::Result<String> {
    use std::os::windows::io::AsRawHandle as _;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    let mut pid: u32 = 0;
    // SAFETY: a live handle from the open above, and a valid out-pointer.
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle() as HANDLE, &mut pid) } == 0 {
        return Err(std::io::Error::last_os_error());
    }

    // The narrowest right that answers the question: enough to read the
    // token, and not enough to do anything to a process this side has just
    // learned the id of.
    // SAFETY: a plain call; the handle is checked below and closed by the
    // guard.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let process = ServerProcess(process);

    // SAFETY: an open process handle carrying the right this asks for.
    let token = unsafe { crate::windows_user::token_of(process.0) }?;
    crate::windows_user::sid_string_of(&token)
}

/// Closes the server's process handle however the caller leaves.
#[cfg(windows)]
struct ServerProcess(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for ServerProcess {
    fn drop(&mut self) {
        // SAFETY: opened above and closed once.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// Refuse a daemon that is not us.
///
/// The socket lives at a predictable path, and where `XDG_RUNTIME_DIR` is
/// unset that path is under `/tmp` — where another local user can create the
/// directory first and bind a socket of their own. The daemon checks the
/// directory it creates; a client that simply connects checks nothing, and
/// would hand its session requests, message text included, to whoever
/// answered. The kernel knows who that is, so ask it.
#[cfg(unix)]
fn check_peer(stream: &Inner) -> std::io::Result<()> {
    let peer = peer_uid(stream)?;
    let us = rustix::process::getuid().as_raw();
    if peer == us {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!(
            "the daemon socket is owned by uid {peer}, not by us ({us}); refusing to talk to it"
        ),
    ))
}

/// Who is on the other end, as the kernel sees it.
///
/// Two calls for one question, because Unix never agreed on it: Linux answers
/// `SO_PEERCRED`, and everyone else answers `getpeereid`. Both are read off
/// the connected socket rather than taken on trust, which is the point.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_uid(stream: &Inner) -> std::io::Result<u32> {
    Ok(rustix::net::sockopt::socket_peercred(stream)?.uid.as_raw())
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn peer_uid(stream: &Inner) -> std::io::Result<u32> {
    use std::os::fd::AsRawFd as _;

    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: a connected socket's fd, and two out-pointers to locals.
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if rc == 0 {
        Ok(uid)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A daemon that stops draining its socket fills the kernel's buffer, and
    /// a send then blocks with nothing to end it — on the caller's own
    /// thread, holding the lock every other send waits on. The window used to
    /// freeze whole, with no timeout and no recovery.
    // A stopwatch in a test, and this crate has no `wacore` to borrow one
    // from — it deliberately depends on nothing that reaches the network.
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn a_send_nobody_is_reading_gives_up_rather_than_hanging() {
        let dir =
            std::env::temp_dir().join(format!("oxidezap-write-timeout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        let path = dir.join("endpoint.sock");

        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        let accepted = std::thread::spawn(move || {
            // Accepted and then never read from, which is the whole scenario.
            listener.accept().map(|(stream, _)| stream)
        });
        let endpoint = Endpoint::connect_at(&path).expect("connect");
        let held = accepted.join().expect("accept thread").expect("accept");

        let (_reader, mut writer) = endpoint.split().expect("split");
        assert_eq!(
            writer.0.write_timeout().expect("read the timeout back"),
            Some(WRITE_TIMEOUT),
            "the write half is given a deadline"
        );
        // Shortened for the fill below, which would otherwise spend the real
        // one twice over on every run.
        writer
            .0
            .set_write_timeout(Some(std::time::Duration::from_millis(200)))
            .expect("shorten the deadline");
        let started = std::time::Instant::now();
        let mut sent = 0usize;
        let payload = vec![b'x'; 64 * 1024];
        let outcome = loop {
            match writer.write(&payload) {
                Ok(n) => sent += n,
                Err(e) => break e,
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(20),
                "the buffer should have filled long before this ({sent} bytes)"
            );
        };
        assert!(
            matches!(
                outcome.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ),
            "a stalled send ends as an ordinary I/O error: {outcome:?}"
        );

        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
