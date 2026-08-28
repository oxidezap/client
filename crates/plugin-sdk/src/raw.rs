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

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "oxidezap")]
unsafe extern "C" {
    #[link_name = "oxi_subscribe"]
    pub safe fn subscribe(mask: i64);
    #[link_name = "oxi_request_caps"]
    pub safe fn request_caps(mask: i64);
    #[link_name = "oxi_set_name"]
    pub fn set_name(ptr: i32, len: i32) -> i32;

    #[link_name = "oxi_field_str"]
    pub fn field_str(ev: i32, field: i32, ptr: i32, cap: i32) -> i32;
    #[link_name = "oxi_field_i64"]
    pub safe fn field_i64(ev: i32, field: i32) -> i64;
    #[link_name = "oxi_field_len"]
    pub safe fn field_len(ev: i32, field: i32) -> i32;
    #[link_name = "oxi_field_at"]
    pub safe fn field_at(ev: i32, field: i32, index: i32) -> i32;

    #[link_name = "oxi_send_text"]
    pub fn send_text(jid: i32, jid_len: i32, text: i32, text_len: i32) -> i32;
    #[link_name = "oxi_send_reply"]
    pub fn send_reply(
        jid: i32,
        jid_len: i32,
        text: i32,
        text_len: i32,
        quoted: i32,
        quoted_len: i32,
    ) -> i32;
    #[link_name = "oxi_mark_read"]
    pub fn mark_read(jid: i32, jid_len: i32, id: i32, id_len: i32) -> i32;
    #[link_name = "oxi_typing"]
    pub fn typing(jid: i32, jid_len: i32, composing: i32) -> i32;
    #[link_name = "oxi_ui_set"]
    pub fn ui_set(ptr: i32, len: i32) -> i32;
    #[link_name = "oxi_kv_get"]
    pub fn kv_get(key: i32, key_len: i32, ptr: i32, cap: i32) -> i32;
    #[link_name = "oxi_kv_set"]
    pub fn kv_set(key: i32, key_len: i32, val: i32, val_len: i32) -> i32;
    #[link_name = "oxi_timer_set"]
    pub safe fn timer_set(delay_ms: i64, token: i64) -> i32;

    #[link_name = "oxi_log"]
    pub fn log_raw(level: i32, ptr: i32, len: i32);
    #[link_name = "oxi_now_ms"]
    pub safe fn now_ms() -> i64;
}

#[cfg(not(target_arch = "wasm32"))]
mod off_target {
    /// What every stub does. Reaching one means a plugin's code is running
    /// somewhere there is no host, which nothing legitimate does.
    fn nowhere() -> ! {
        panic!("an oxidezap plugin's imports exist only inside the daemon's wasm host")
    }

    pub fn subscribe(_mask: i64) {
        nowhere()
    }
    pub fn request_caps(_mask: i64) {
        nowhere()
    }
    pub unsafe fn set_name(_ptr: i32, _len: i32) -> i32 {
        nowhere()
    }
    pub unsafe fn field_str(_ev: i32, _field: i32, _ptr: i32, _cap: i32) -> i32 {
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
    pub unsafe fn send_text(_jid: i32, _jid_len: i32, _text: i32, _text_len: i32) -> i32 {
        nowhere()
    }
    pub unsafe fn send_reply(
        _jid: i32,
        _jid_len: i32,
        _text: i32,
        _text_len: i32,
        _quoted: i32,
        _quoted_len: i32,
    ) -> i32 {
        nowhere()
    }
    pub unsafe fn mark_read(_jid: i32, _jid_len: i32, _id: i32, _id_len: i32) -> i32 {
        nowhere()
    }
    pub unsafe fn typing(_jid: i32, _jid_len: i32, _composing: i32) -> i32 {
        nowhere()
    }
    pub unsafe fn ui_set(_ptr: i32, _len: i32) -> i32 {
        nowhere()
    }
    pub unsafe fn kv_get(_key: i32, _key_len: i32, _ptr: i32, _cap: i32) -> i32 {
        nowhere()
    }
    pub unsafe fn kv_set(_key: i32, _key_len: i32, _val: i32, _val_len: i32) -> i32 {
        nowhere()
    }
    pub fn timer_set(_delay_ms: i64, _token: i64) -> i32 {
        nowhere()
    }
    pub unsafe fn log_raw(_level: i32, _ptr: i32, _len: i32) {
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
    unsafe { log_raw(level, line.as_ptr() as i32, line.len() as i32) };
}

/// The log levels, for a caller that would rather not reach into `abi`.
pub mod level {
    use super::abi;

    pub const ERROR: i32 = abi::log::ERROR;
    pub const WARN: i32 = abi::log::WARN;
    pub const INFO: i32 = abi::log::INFO;
    pub const DEBUG: i32 = abi::log::DEBUG;
}
