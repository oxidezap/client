//! Windows named-pipe I/O that does not serialize the two directions.
//!
//! A handle opened without `FILE_FLAG_OVERLAPPED` is a *synchronous* handle,
//! and Windows serializes every operation on one: while a `ReadFile` is
//! pending, a `WriteFile` blocks — from any thread, and through a duplicated
//! handle, because the serialization belongs to the file object rather than to
//! the handle naming it.
//!
//! That is a deadlock for this protocol and not a slow path. The reader parks
//! in `ReadFile` waiting for the daemon's next frame, which is the steady
//! state; a request written from another thread then waits for a read that is
//! waiting for that request. Nothing arrives, so nothing returns.
//!
//! So the handle is opened overlapped and every call passes an `OVERLAPPED` —
//! all of them, because a handle opened this way and used synchronously reads
//! zero bytes and hangs in its own way. The calls still *look* blocking to
//! everything above: each waits on its own event until its own operation
//! finishes, which is what a reader thread and a writing UI thread each want.
//! What they no longer do is wait on each other.

use std::io;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED, ERROR_PIPE_NOT_CONNECTED, HANDLE,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects,
};

/// The flag that makes a handle overlapped, for the caller that opens it.
pub use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

/// One end of an overlapped pipe, with the event its own operations wait on.
///
/// The event is per end, not per handle: a reader and a writer run at the same
/// time by design, and two operations sharing one event would each be woken by
/// the other's completion.
pub struct Overlapped {
    pipe: std::fs::File,
    event: OwnedHandle,
    stop: Stop,
}

/// What ends a wait from another thread.
///
/// A pipe cannot be shut down the way a socket can, so the wait is on the
/// operation's own event *and* on this one: signalling it cancels the pending
/// read and the call reports end of file. Shared rather than duplicated,
/// because the point is to reach an end somebody else is holding.
#[derive(Clone)]
pub struct Stop(Arc<OwnedHandle>);

impl Stop {
    pub fn signal(&self) {
        // SAFETY: an event this type owns; failing to set it only means the
        // reader waits for the daemon instead, which is what it did before.
        unsafe { SetEvent(self.0.as_raw_handle() as HANDLE) };
    }
}

impl Overlapped {
    pub fn new(pipe: std::fs::File) -> io::Result<Self> {
        Ok(Self {
            pipe,
            event: manual_reset_event()?,
            stop: Stop(Arc::new(manual_reset_event()?)),
        })
    }

    /// A second end on the same pipe, with an event of its own.
    ///
    /// The stop is shared: it belongs to the connection rather than to one
    /// end of it, and a hangup taken from either has to reach the read.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            pipe: self.pipe.try_clone()?,
            event: manual_reset_event()?,
            stop: self.stop.clone(),
        })
    }

    /// The handle that ends this end's wait.
    pub fn stop(&self) -> Stop {
        self.stop.clone()
    }

    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `buf` is valid for `len` bytes and outlives the call, which
        // does not return until the operation it started has finished.
        let read = unsafe {
            self.perform(|handle, ov| {
                ReadFile(
                    handle,
                    buf.as_mut_ptr(),
                    clamp(buf.len()),
                    std::ptr::null_mut(),
                    ov,
                )
            })
        };
        match read {
            // A pipe whose other end has gone reports `ERROR_BROKEN_PIPE`,
            // and one this side hung up on reports `ERROR_OPERATION_ABORTED`.
            // For a stream both are end of file — the same thing a Unix
            // socket says by returning zero, and what the `BufRead` above
            // needs to see to stop.
            Err(e) if hung_up(&e) => Ok(0),
            other => other,
        }
    }

    pub fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // SAFETY: as above; the buffer is only read.
        //
        // A broken pipe stays an error here, unlike in `read`: there is no
        // such thing as writing end-of-file, and reporting zero would have
        // `write_all` call it a `WriteZero` rather than the disconnection it
        // is.
        unsafe {
            self.perform(|handle, ov| {
                WriteFile(
                    handle,
                    buf.as_ptr(),
                    clamp(buf.len()),
                    std::ptr::null_mut(),
                    ov,
                )
            })
        }
    }

    /// Nothing is buffered on this side: a completed write has been handed to
    /// the pipe. Present because `Write` asks for it.
    pub fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Start one operation and wait for that one to finish.
    ///
    /// # Safety
    ///
    /// `start` must be a `ReadFile`/`WriteFile` over a buffer that stays valid
    /// and untouched until this returns — which is the whole call, since it
    /// waits.
    unsafe fn perform(
        &mut self,
        start: impl FnOnce(HANDLE, *mut OVERLAPPED) -> windows_sys::core::BOOL,
    ) -> io::Result<usize> {
        let handle = self.pipe.as_raw_handle() as HANDLE;
        let event = self.event.as_raw_handle() as HANDLE;

        // Manual-reset and reused, so it has to start unsignalled or the wait
        // below would return on the *previous* operation's completion.
        // SAFETY: an event this type owns.
        if unsafe { ResetEvent(event) } == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut overlapped = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };

        // The closure is safe to call; what it does inside is the caller's
        // contract. The `OVERLAPPED` lives on this stack frame until the wait
        // below has completed the operation, which is what makes handing out
        // a pointer to it sound.
        let started = start(handle, &raw mut overlapped);
        if started == 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(e);
            }
        }

        // Two things end this wait: the operation finishing, and somebody
        // hanging up. A pipe has no shutdown, so the second is an event —
        // and once it fires the pending operation is cancelled so the
        // completion below has something to report.
        let waits = [event, self.stop.0.as_raw_handle() as HANDLE];
        // SAFETY: two events this type owns, waited on for as long as it
        // takes one of them to fire.
        let woken = unsafe { WaitForMultipleObjects(2, waits.as_ptr(), 0, INFINITE) };
        if woken == WAIT_OBJECT_0 + 1 {
            // SAFETY: the operation above was started on this handle with
            // this `OVERLAPPED`, from this thread.
            unsafe { CancelIoEx(handle, &overlapped) };
        }

        let mut transferred = 0u32;
        // SAFETY: as above; `TRUE` waits for the operation to have finished
        // cancelling rather than polling.
        let ok = unsafe { GetOverlappedResult(handle, &overlapped, &mut transferred, 1) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(transferred as usize)
    }
}

/// Whether this error means the other end is gone.
fn hung_up(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error().map(|code| code as u32),
        Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_OPERATION_ABORTED)
    )
}

fn manual_reset_event() -> io::Result<OwnedHandle> {
    // SAFETY: no attributes, no name; a manual-reset event starting
    // unsignalled.
    let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh handle this function is the sole owner of.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.cast()) })
}

/// One call moves at most `u32::MAX` bytes; a longer buffer takes another.
///
/// `Read`/`Write` are allowed to move less than asked, and everything above
/// this loops.
fn clamp(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}
