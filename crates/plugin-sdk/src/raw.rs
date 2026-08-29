//! The imports themselves, as declared to the linker.
//!
//! Every one is `unsafe` because it takes a pointer and a length the caller
//! is trusting itself to have got right; everything above this module exists
//! so that nothing else has to.
//!
//! # Why there are two copies of each
//!
//! A plugin is built for `wasm32-unknown-unknown`, where these resolve
//! against the host's `oxidezap` module. Everywhere else there is no such
//! module and a binary that linked them would not link at all — which would
//! mean this crate could not be a workspace member, and so would never be
//! compiled by CI. The stubs are what let `cargo clippy --workspace` check
//! this code on an ordinary machine. Calling one is a bug in the caller: a
//! plugin's functions only ever run inside the sandbox.

#![allow(clippy::missing_safety_doc)]

use oxidezap_plugin_abi as abi;

/// How an address crosses this ABI.
///
/// `i32` on wasm32, where the whole address space is 32 bits and that is what
/// the wire says. Everything else is a *host*, where an address does not fit
/// in an `i32` and casting one there truncates it into a pointer to nothing —
/// which is exactly what the test host does with it, so the width has to be
/// the target's rather than the protocol's.
#[cfg(target_arch = "wasm32")]
pub type Ptr = i32;
#[cfg(not(target_arch = "wasm32"))]
pub type Ptr = usize;

/// The address of some bytes, at this target's width.
#[must_use]
pub fn at(bytes: &[u8]) -> Ptr {
    bytes.as_ptr() as Ptr
}

/// The same, for a buffer something is about to be written into.
#[must_use]
pub fn into(bytes: &mut [u8]) -> Ptr {
    bytes.as_mut_ptr() as Ptr
}

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "oxidezap")]
unsafe extern "C" {
    #[link_name = "oxi_subscribe"]
    pub safe fn subscribe(mask: i64);
    #[link_name = "oxi_request_caps"]
    pub safe fn request_caps(mask: i64);
    #[link_name = "oxi_set_name"]
    pub fn set_name(ptr: Ptr, len: i32) -> i32;

    #[link_name = "oxi_field_str"]
    pub fn field_str(ev: i32, field: i32, ptr: Ptr, cap: i32) -> i32;
    #[link_name = "oxi_field_i64"]
    pub safe fn field_i64(ev: i32, field: i32) -> i64;
    #[link_name = "oxi_field_len"]
    pub safe fn field_len(ev: i32, field: i32) -> i32;
    #[link_name = "oxi_field_at"]
    pub safe fn field_at(ev: i32, field: i32, index: i32) -> i32;

    #[link_name = "oxi_send_text"]
    pub fn send_text(jid: Ptr, jid_len: i32, text: Ptr, text_len: i32) -> i32;
    #[link_name = "oxi_send_reply"]
    pub fn send_reply(
        jid: Ptr,
        jid_len: i32,
        text: Ptr,
        text_len: i32,
        quoted: Ptr,
        quoted_len: i32,
    ) -> i32;
    #[link_name = "oxi_mark_read"]
    pub fn mark_read(jid: Ptr, jid_len: i32, id: Ptr, id_len: i32) -> i32;
    #[link_name = "oxi_typing"]
    pub fn typing(jid: Ptr, jid_len: i32, composing: i32) -> i32;
    #[link_name = "oxi_ui_set"]
    pub fn ui_set(ptr: Ptr, len: i32) -> i32;
    #[link_name = "oxi_kv_get"]
    pub fn kv_get(key: Ptr, key_len: i32, ptr: Ptr, cap: i32) -> i32;
    #[link_name = "oxi_kv_set"]
    pub fn kv_set(key: Ptr, key_len: i32, val: Ptr, val_len: i32) -> i32;
    #[link_name = "oxi_timer_set"]
    pub safe fn timer_set(delay_ms: i64, token: i64) -> i32;

    #[link_name = "oxi_log"]
    pub fn log_raw(level: i32, ptr: Ptr, len: i32);
    #[link_name = "oxi_now_ms"]
    pub safe fn now_ms() -> i64;
}

#[cfg(all(not(target_arch = "wasm32"), feature = "testing"))]
mod off_target {
    //! Answered by [`crate::testing`], so a plugin's handlers can be run by
    //! `cargo test` on an ordinary machine. Not the daemon: nothing here
    //! enforces fuel, capabilities or approval.

    use crate::testing;
    use std::borrow::ToOwned as _;
    use std::string::String;

    /// The shape every string-returning import has: write `min(cap, len)`
    /// bytes and answer the *full* length, so the caller can tell a value
    /// that fit from one that was cut.
    unsafe fn write_out(value: Option<String>, ptr: super::Ptr, cap: i32) -> i32 {
        let Some(value) = value else {
            return oxidezap_plugin_abi::ABSENT;
        };
        let cap = usize::try_from(cap).unwrap_or(0);
        let wrote = value.len().min(cap);
        if wrote > 0 {
            // SAFETY: the caller owns `cap` bytes at `ptr`, which is the
            // contract every one of these imports is called under.
            unsafe {
                core::ptr::copy_nonoverlapping(value.as_ptr(), ptr as *mut u8, wrote);
            }
        }
        i32::try_from(value.len()).unwrap_or(i32::MAX)
    }

    /// A `&str` the guest handed over as a pointer and a length.
    unsafe fn borrow<'a>(ptr: super::Ptr, len: i32) -> &'a str {
        let len = usize::try_from(len).unwrap_or(0);
        // SAFETY: as above — and a plugin's strings are `&str` on this side
        // of the call, so they are valid UTF-8 by construction.
        unsafe {
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr as *const u8, len))
        }
    }

    pub fn subscribe(mask: i64) {
        testing::subscribe(mask);
    }
    pub fn request_caps(mask: i64) {
        testing::request_caps(mask);
    }
    pub unsafe fn set_name(ptr: super::Ptr, len: i32) -> i32 {
        testing::set_name(unsafe { borrow(ptr, len) })
    }
    pub unsafe fn field_str(ev: i32, field: i32, ptr: super::Ptr, cap: i32) -> i32 {
        unsafe { write_out(testing::field_str(ev, field), ptr, cap) }
    }
    pub fn field_i64(ev: i32, field: i32) -> i64 {
        testing::field_i64(ev, field)
    }
    pub fn field_len(ev: i32, field: i32) -> i32 {
        testing::field_len(ev, field)
    }
    pub fn field_at(ev: i32, field: i32, index: i32) -> i32 {
        testing::field_at(ev, field, index)
    }
    pub unsafe fn send_text(jid: super::Ptr, jid_len: i32, text: super::Ptr, text_len: i32) -> i32 {
        testing::command(testing::Command::Send {
            chat: unsafe { borrow(jid, jid_len) }.to_owned(),
            text: unsafe { borrow(text, text_len) }.to_owned(),
        })
    }
    pub unsafe fn send_reply(
        jid: super::Ptr,
        jid_len: i32,
        text: super::Ptr,
        text_len: i32,
        quoted: super::Ptr,
        quoted_len: i32,
    ) -> i32 {
        testing::command(testing::Command::Reply {
            chat: unsafe { borrow(jid, jid_len) }.to_owned(),
            text: unsafe { borrow(text, text_len) }.to_owned(),
            quoted: unsafe { borrow(quoted, quoted_len) }.to_owned(),
        })
    }
    pub unsafe fn mark_read(jid: super::Ptr, jid_len: i32, id: super::Ptr, id_len: i32) -> i32 {
        let message = unsafe { borrow(id, id_len) };
        testing::command(testing::Command::MarkRead {
            chat: unsafe { borrow(jid, jid_len) }.to_owned(),
            message: (!message.is_empty()).then(|| message.to_owned()),
        })
    }
    pub unsafe fn typing(jid: super::Ptr, jid_len: i32, composing: i32) -> i32 {
        testing::command(testing::Command::Typing {
            chat: unsafe { borrow(jid, jid_len) }.to_owned(),
            composing: composing != 0,
        })
    }
    pub unsafe fn ui_set(ptr: super::Ptr, len: i32) -> i32 {
        let len = usize::try_from(len).unwrap_or(0);
        // SAFETY: as above.
        let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
        testing::command(testing::Command::Ui(bytes.to_vec()))
    }
    pub unsafe fn kv_get(key: super::Ptr, key_len: i32, ptr: super::Ptr, cap: i32) -> i32 {
        let key = unsafe { borrow(key, key_len) };
        unsafe { write_out(testing::kv_get(key), ptr, cap) }
    }
    pub unsafe fn kv_set(key: super::Ptr, key_len: i32, val: super::Ptr, val_len: i32) -> i32 {
        testing::kv_set(unsafe { borrow(key, key_len) }, unsafe {
            borrow(val, val_len)
        })
    }
    pub fn timer_set(delay_ms: i64, token: i64) -> i32 {
        testing::command(testing::Command::Timer { delay_ms, token })
    }
    pub unsafe fn log_raw(_level: i32, _ptr: super::Ptr, _len: i32) {}
    pub fn now_ms() -> i64 {
        testing::now_ms()
    }
}

#[cfg(all(not(target_arch = "wasm32"), not(feature = "testing")))]
mod off_target {
    /// What every stub does. Reaching one means a plugin's code is running
    /// somewhere there is no host, which nothing legitimate does — build with
    /// the `testing` feature to have these answered instead.
    fn nowhere() -> ! {
        panic!("an oxidezap plugin's imports exist only inside the daemon's wasm host")
    }

    pub fn subscribe(_mask: i64) {
        nowhere()
    }
    pub fn request_caps(_mask: i64) {
        nowhere()
    }
    pub unsafe fn set_name(_ptr: super::Ptr, _len: i32) -> i32 {
        nowhere()
    }
    pub unsafe fn field_str(_ev: i32, _field: i32, _ptr: super::Ptr, _cap: i32) -> i32 {
        nowhere()
    }
    pub fn field_i64(_ev: i32, _field: i32) -> i64 {
        nowhere()
    }
    pub fn field_len(_ev: i32, _field: i32) -> i32 {
        nowhere()
    }
    pub fn field_at(_ev: i32, _field: i32, _index: i32) -> i32 {
        nowhere()
    }
    pub unsafe fn send_text(
        _jid: super::Ptr,
        _jid_len: i32,
        _text: super::Ptr,
        _text_len: i32,
    ) -> i32 {
        nowhere()
    }
    pub unsafe fn send_reply(
        _jid: super::Ptr,
        _jid_len: i32,
        _text: super::Ptr,
        _text_len: i32,
        _quoted: super::Ptr,
        _quoted_len: i32,
    ) -> i32 {
        nowhere()
    }
    pub unsafe fn mark_read(_jid: super::Ptr, _jid_len: i32, _id: super::Ptr, _id_len: i32) -> i32 {
        nowhere()
    }
    pub unsafe fn typing(_jid: super::Ptr, _jid_len: i32, _composing: i32) -> i32 {
        nowhere()
    }
    pub unsafe fn ui_set(_ptr: super::Ptr, _len: i32) -> i32 {
        nowhere()
    }
    pub unsafe fn kv_get(_key: super::Ptr, _key_len: i32, _ptr: super::Ptr, _cap: i32) -> i32 {
        nowhere()
    }
    pub unsafe fn kv_set(_key: super::Ptr, _key_len: i32, _val: super::Ptr, _val_len: i32) -> i32 {
        nowhere()
    }
    pub fn timer_set(_delay_ms: i64, _token: i64) -> i32 {
        nowhere()
    }
    pub unsafe fn log_raw(_level: i32, _ptr: super::Ptr, _len: i32) {
        nowhere()
    }
    pub fn now_ms() -> i64 {
        nowhere()
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use off_target::*;

/// Write a line into the daemon's log, prefixed with this plugin's id.
///
/// Not behind a capability: it reveals nothing the plugin was not already
/// handed, and a plugin that cannot say what went wrong is one nobody can
/// debug.
pub fn log(level: i32, line: &str) {
    // `as i32` is how every pointer crosses this ABI: wasm32 addresses are
    // 32-bit, so nothing is lost.
    unsafe { log_raw(level, at(line.as_bytes()), line.len() as i32) };
}

/// The log levels, for a caller that would rather not reach into `abi`.
pub mod level {
    use super::abi;

    pub const ERROR: i32 = abi::log::ERROR;
    pub const WARN: i32 = abi::log::WARN;
    pub const INFO: i32 = abi::log::INFO;
    pub const DEBUG: i32 = abi::log::DEBUG;
    pub const TRACE: i32 = abi::log::TRACE;
}
