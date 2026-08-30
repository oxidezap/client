//! Who this process is, as Windows counts identity.
//!
//! A named pipe name is machine-wide, so it has to say whose pipe it is, and
//! `USERNAME` does not answer that: two accounts from different domains can
//! share one, and a process controls its own environment. The token's SID is
//! the identity the kernel itself uses, which is also what the daemon's
//! access-control entry names — so both come from here rather than each side
//! asking separately.

use std::io;

use windows_sys::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// The process token's `TOKEN_USER`, SID and all.
///
/// Returned as bytes because the SID lives past the end of the struct, and
/// splitting them would hand out a pointer into a buffer the caller does not
/// own. Whoever wants the SID reads it out of this and keeps it alive.
pub fn token() -> io::Result<Vec<u8>> {
    // SAFETY: the pseudo-handle names this process and needs no closing.
    unsafe { token_of(GetCurrentProcess()) }
}

/// [`token`] for a process this one has opened.
///
/// The same question asked of somebody else, which is what a client needs to
/// know who answered its pipe.
///
/// # Safety
///
/// `process` must be an open process handle carrying
/// `PROCESS_QUERY_LIMITED_INFORMATION`.
pub unsafe fn token_of(process: HANDLE) -> io::Result<Vec<u8>> {
    let mut handle: HANDLE = std::ptr::null_mut();
    // SAFETY: a valid process handle from the caller, and a valid
    // out-pointer.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut handle) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let handle = OwnedHandle(handle);

    let mut needed = 0u32;
    // SAFETY: asking for the size first is how this call is specified; it
    // fails with `ERROR_INSUFFICIENT_BUFFER` and sets `needed`.
    unsafe {
        GetTokenInformation(handle.0, TokenUser, std::ptr::null_mut(), 0, &mut needed);
    }
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut buffer = vec![0u8; needed as usize];
    // SAFETY: the buffer is exactly the size the call just asked for.
    let ok = unsafe {
        GetTokenInformation(
            handle.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        ) != 0
    };
    if ok {
        Ok(buffer)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The SID inside a buffer [`token`] returned.
///
/// # Safety
///
/// `token` must be a buffer this module produced, and must outlive the
/// returned pointer.
pub unsafe fn sid_of(token: &[u8]) -> *mut core::ffi::c_void {
    // SAFETY: the caller guarantees the buffer's provenance and lifetime, and
    // `GetTokenInformation` writes a `TOKEN_USER` at its start.
    //
    // Read *unaligned*, which is the half this used to leave out: the buffer
    // is a `Vec<u8>` and so has an alignment of one, while `TOKEN_USER`
    // holds a pointer and wants eight. Dereferencing a `*const TOKEN_USER`
    // from there is an unaligned read — undefined behaviour by Rust's rules
    // whatever the allocator happens to hand back — and one of the two
    // callers builds the named pipe's security descriptor out of the answer.
    unsafe {
        std::ptr::read_unaligned(token.as_ptr().cast::<TOKEN_USER>())
            .User
            .Sid
    }
}

/// This user's SID in its textual form, `S-1-5-21-...`.
///
/// Stable, unique, and short enough to put in a pipe name.
pub fn sid_string() -> io::Result<String> {
    // SAFETY: the buffer is one `token` just produced.
    unsafe { sid_string_of(&token()?) }
}

/// [`sid_string`] for a token this module produced, whosever it is.
///
/// # Safety
///
/// `token` must be a buffer [`token`] or [`token_of`] returned. It is read as
/// a `TOKEN_USER`, whose `Sid` points past the end of the struct, so an
/// arbitrary slice — empty, short, or simply somebody else's bytes — is a
/// pointer this dereferences before Windows can refuse it. Safe on its face
/// and unsafe in fact is the one shape this must not have: the caller is what
/// knows where the buffer came from.
pub unsafe fn sid_string_of(token: &[u8]) -> io::Result<String> {
    // SAFETY: the caller guarantees the buffer's provenance, and it outlives
    // this block.
    let sid = unsafe { sid_of(token) };

    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: a SID from the token above, and a valid out-pointer.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let owned = Wide(text);

    // SAFETY: `ConvertSidToStringSidW` returns a NUL-terminated wide string.
    let len = unsafe {
        let mut len = 0;
        while *owned.0.add(len) != 0 {
            len += 1;
        }
        len
    };
    // SAFETY: `len` units, all initialized, from the pointer above.
    let slice = unsafe { std::slice::from_raw_parts(owned.0, len) };
    Ok(String::from_utf16_lossy(slice))
}

/// Closes the token handle however the caller leaves.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: opened above and closed once.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
        }
    }
}

/// Frees a wide string Windows allocated with `LocalAlloc`.
struct Wide(*mut u16);

impl Drop for Wide {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: allocated by `ConvertSidToStringSidW`, freed once.
            unsafe { LocalFree(self.0.cast::<HLOCAL>() as HLOCAL) };
        }
    }
}
