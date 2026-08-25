//! A security descriptor naming only the user who runs the daemon.
//!
//! A named pipe created with the default descriptor grants read access to
//! `Everyone` and the anonymous account. This endpoint carries the session
//! stream — message text, chat names, the keys to the media cache — so the
//! default is the wrong one, and Windows offers no way to ask for "just me"
//! other than building the descriptor.
//!
//! The Unix side has no equivalent because it does not need one: the socket
//! lives in a directory the daemon creates mode `0700` and verifies it owns.

use std::io;

use windows_sys::Win32::Foundation::{HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, SET_ACCESS, SetEntriesInAclW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACL, InitializeSecurityDescriptor, NO_INHERITANCE, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    SECURITY_DESCRIPTOR, SetSecurityDescriptorDacl,
};
use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
use windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;

/// A `SECURITY_ATTRIBUTES` whose DACL grants this user and nobody else.
///
/// Everything it points at is owned here and stays put for as long as it
/// lives, which is what makes handing out a raw pointer to it defensible.
pub struct UserOnly {
    attributes: SECURITY_ATTRIBUTES,
    /// Boxed because `attributes` points into it: moving `UserOnly` must not
    /// move the descriptor out from under that pointer.
    descriptor: Box<SECURITY_DESCRIPTOR>,
    /// Allocated by `SetEntriesInAclW` with `LocalAlloc`, referenced by the
    /// descriptor, and freed on drop.
    acl: *mut ACL,
    /// The token information holding the SID the ACL names.
    _token: Vec<u8>,
}

impl UserOnly {
    pub fn new() -> io::Result<Self> {
        // The same token the endpoint's name is derived from, so the pipe and
        // the entry that guards it name one identity rather than two.
        let token = oxidezap_ipc::windows_user::token()?;
        // SAFETY: the buffer came from that call and outlives every use below.
        let sid = unsafe { oxidezap_ipc::windows_user::sid_of(&token) };

        let mut access = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: SET_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                // The SID is read, not written, despite the field's type.
                ptstrName: sid.cast(),
            },
        };

        let mut acl: *mut ACL = std::ptr::null_mut();
        // SAFETY: one initialized entry, no ACL to merge with, and `acl` is a
        // valid out-pointer.
        let status = unsafe { SetEntriesInAclW(1, &mut access, std::ptr::null(), &mut acl) };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }

        let mut descriptor: Box<SECURITY_DESCRIPTOR> = Box::new(unsafe { std::mem::zeroed() });
        let raw: PSECURITY_DESCRIPTOR = std::ptr::from_mut(descriptor.as_mut()).cast();
        // SAFETY: `raw` points at a descriptor-sized allocation that lives as
        // long as `self`, and `acl` is the one just built.
        let ok = unsafe {
            InitializeSecurityDescriptor(raw, SECURITY_DESCRIPTOR_REVISION) != 0
                && SetSecurityDescriptorDacl(raw, 1, acl, 0) != 0
        };
        if !ok {
            let e = io::Error::last_os_error();
            // SAFETY: `acl` came from `SetEntriesInAclW` and is freed once.
            unsafe { LocalFree(acl.cast::<HLOCAL>() as HLOCAL) };
            return Err(e);
        }

        Ok(Self {
            attributes: SECURITY_ATTRIBUTES {
                nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
                lpSecurityDescriptor: raw,
                bInheritHandle: 0,
            },
            descriptor,
            acl,
            _token: token,
        })
    }

    /// The attributes, for a call that wants them raw.
    ///
    /// Valid for as long as `self` is.
    pub fn as_ptr(&self) -> *mut core::ffi::c_void {
        std::ptr::from_ref(&self.attributes)
            .cast::<core::ffi::c_void>()
            .cast_mut()
    }
}

impl Drop for UserOnly {
    fn drop(&mut self) {
        // The descriptor points at the ACL, so the ACL goes second — and both
        // outlive every pipe created from them, because the kernel copies the
        // descriptor when the pipe is made.
        let _ = &self.descriptor;
        if !self.acl.is_null() {
            // SAFETY: allocated by `SetEntriesInAclW`, freed once, and
            // nothing else holds it.
            unsafe { LocalFree(self.acl.cast::<HLOCAL>() as HLOCAL) };
        }
    }
}
