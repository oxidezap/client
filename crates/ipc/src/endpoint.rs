//! Connecting to the daemon, on whatever local transport the platform has.
//!
//! A Unix socket where there is one, a named pipe on Windows. Both are
//! byte streams a client reads and writes with `std::io`, so everything above
//! this — the framing, the requests, the whole protocol — is the same code on
//! both, and a front end never mentions either.
//!
//! Blocking, and deliberately: a newline-delimited protocol over a local
//! transport needs one thread to read and a lock to serialize writes, not a
//! runtime. The daemon is the side that has thousands of things happening at
//! once; a client has one.

use std::io::{Read, Write};

/// A connected, blocking, duplex stream to the daemon.
pub struct Endpoint(Inner);

#[cfg(unix)]
type Inner = std::os::unix::net::UnixStream;

/// A named pipe opened as a file.
///
/// Windows has no `std` named-pipe client, but a pipe *is* openable by name
/// with the ordinary file API, and the handle it returns is the duplex stream
/// this needs. Nothing above cares which one it got.
#[cfg(windows)]
type Inner = std::fs::File;

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

        #[cfg(unix)]
        {
            let stream = Inner::connect(&path)?;
            check_peer(&stream)?;
            Ok(Self(stream))
        }
        #[cfg(windows)]
        {
            // Read and write, because the pipe is duplex and opening it for
            // one direction would half-connect.
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map(Self)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            Err(std::io::Error::other("no local transport on this platform"))
        }
    }

    /// A second handle on the same connection.
    ///
    /// One thread reads while the rest of the program writes; both need the
    /// stream, and neither may own it exclusively.
    pub fn try_clone(&self) -> std::io::Result<Self> {
        self.0.try_clone().map(Self)
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

impl Read for Endpoint {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Endpoint {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}
