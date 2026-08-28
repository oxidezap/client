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
