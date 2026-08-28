//! Write an oxidezap plugin in Rust.
//!
//! The raw ABI is four read functions, a handful of commands and three
//! exports; this crate is the thin, typed layer over it and nothing more.
//! There is no runtime here, no allocator, no `std` — a plugin built on this
//! carries the bytes of its own logic and almost nothing else, which is the
//! entire point of the ABI it wraps.
//!
//! ```ignore
//! use oxidezap_plugin::{abi, plugin, Event, send_text, Text};
//!
//! plugin!(init = setup, event = handle);
//!
//! fn setup(p: &mut Setup) {
//!     p.name("Autoreply");
//!     p.subscribe(abi::kinds::bit(abi::kinds::MESSAGE));
//!     p.capabilities(abi::caps::SEND);
//! }
//!
//! fn handle(ev: &Event) {
//!     if ev.kind() != abi::kinds::MESSAGE || ev.flag(abi::fields::FROM_ME) {
//!         return;
//!     }
//!     let text = ev.text::<256>(abi::fields::TEXT);
//!     if text.as_str().contains("ping") {
//!         let chat = ev.text::<128>(abi::fields::CHAT_JID);
//!         send_text(chat.as_str(), "pong");
//!     }
//! }
//! ```
//!
//! # Reading without allocating
//!
//! Strings come back through [`Text`], a fixed-size buffer on the stack. This
//! is not asceticism: a plugin with no allocator is tens of kilobytes and one
//! with a heap is not, and the ABI is arranged so the smaller answer is also
//! the natural one. A value longer than the buffer is truncated at a
//! character boundary and says so through [`Text::complete`].
//!
//! # Building one
//!
//! ```text
//! cargo build --release --target wasm32-unknown-unknown
//! ```
//!
//! Then drop the `.wasm` into the plugin directory. The file's name is the
//! plugin's id.

#![no_std]

pub use oxidezap_plugin_abi as abi;

mod raw;

pub use raw::{level, log, now_ms};

/// What a command answered.
///
/// A plugin learns this where a socket front end does not: the call is
/// synchronous, so there is no request to correlate an answer with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The session took it. What the network makes of it arrives as an event.
    Accepted,
    /// There was no session to carry it out. Worth retrying later.
    NoSession,
    /// The daemon will not do this as asked. Not worth retrying unchanged.
    Refused,
    /// This plugin did not ask for the capability the command needs.
    Denied,
    /// The arguments did not make sense.
    Invalid,
    /// Right command, wrong moment.
    ///
    /// An account command attempted during `oxi_init`, or one more widget
    /// tree than a single call may publish. Distinct from [`Invalid`] and
    /// from [`Denied`] on purpose: nothing about the call was wrong and the
    /// plugin may well be allowed to make it — it is too early, or too often,
    /// and the same call from a handler is fine.
    ///
    /// [`Invalid`]: Self::Invalid
    /// [`Denied`]: Self::Denied
    State,
}

impl Outcome {
    fn of(code: i32) -> Self {
        match code {
            abi::outcome::ACCEPTED => Self::Accepted,
            abi::outcome::NO_SESSION => Self::NoSession,
            abi::outcome::DENIED => Self::Denied,
            abi::outcome::REFUSED => Self::Refused,
            abi::outcome::STATE => Self::State,
            _ => Self::Invalid,
        }
    }

    /// Whether the session took it.
    #[must_use]
    pub fn is_accepted(self) -> bool {
        self == Self::Accepted
    }
}

/// A string read out of an event, held on the stack.
///
/// `N` is the room made for it. A value that does not fit is truncated at a
/// character boundary rather than at a byte, so what comes back is always a
/// shorter string and never bytes that are not one.
pub struct Text<const N: usize> {
    buf: [u8; N],
    /// How long the value really is, which can exceed `N`.
    full: usize,
    /// How much of it is in `buf`.
    len: usize,
    present: bool,
}

impl<const N: usize> Text<N> {
    fn absent() -> Self {
        Self {
            buf: [0; N],
            full: 0,
            len: 0,
            present: false,
        }
    }

    /// What was read, possibly truncated. Empty when the field was absent.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // The host writes UTF-8 and this truncates on a character boundary,
        // so the only way to be here with invalid bytes is a host that broke
        // its own contract. An empty string is the answer that cannot make a
        // plugin act on garbage.
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }

    /// Whether the field was there at all, which is not the same as it being
    /// non-empty.
    #[must_use]
    pub fn present(&self) -> bool {
        self.present
    }

    /// Whether the whole value fit.
    ///
    /// Worth checking wherever a truncated value would be *wrong* rather than
    /// merely short — a JID being the obvious one, where the first 32 bytes
    /// of a longer one names somebody else entirely.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.full <= N
    }

    /// How long the value is, whether or not it fit.
    #[must_use]
    pub fn full_len(&self) -> usize {
        self.full
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const N: usize> core::fmt::Debug for Text<N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The event being handled.
///
/// A handle, not a struct: nothing is read until a field is asked for, so a
/// handler that looks at the text and the chat pays for two strings out of an
/// event that carries a dozen.
pub struct Event {
    kind: i32,
    handle: i32,
}

impl Event {
    /// Wrap what `oxi_on_event` was handed. Called by [`plugin!`].
    #[must_use]
    pub fn new(kind: i32, handle: i32) -> Self {
        Self { kind, handle }
    }

    /// Which kind this is: one of `abi::kinds`.
    #[must_use]
    pub fn kind(&self) -> i32 {
        self.kind
    }

    /// Read a string field into `N` bytes of stack.
    #[must_use]
    pub fn text<const N: usize>(&self, field: i32) -> Text<N> {
        read_into(|ptr, cap| unsafe { raw::field_str(self.handle, field, ptr, cap) })
    }

    /// Read an integer field. Absent reads back as `0`, by the ABI's absence
    /// rule.
    #[must_use]
    pub fn int(&self, field: i32) -> i64 {
        raw::field_i64(self.handle, field)
    }

    /// Read a boolean field. Absent reads back as `false`.
    #[must_use]
    pub fn flag(&self, field: i32) -> bool {
        self.int(field) != 0
    }

    /// How many elements a repeated field has.
    #[must_use]
    pub fn count(&self, field: i32) -> usize {
        let n = raw::field_len(self.handle, field);
        if n < 0 { 0 } else { n as usize }
    }

    /// One element of a repeated field, as a string.
    #[must_use]
    pub fn at<const N: usize>(&self, field: i32, index: usize) -> Text<N> {
        let index = if index > i32::MAX as usize {
            return Text::absent();
        } else {
            index as i32
        };
        let child = raw::field_at(self.handle, field, index);
        if child == abi::ABSENT {
            return Text::absent();
        }
        read_into(|ptr, cap| unsafe { raw::field_str(child, abi::fields::SELF, ptr, cap) })
    }
}

/// The shared shape of every "write into my buffer and tell me the real
/// length" call.
fn read_into<const N: usize>(mut call: impl FnMut(i32, i32) -> i32) -> Text<N> {
    let mut out = Text::<N>::absent();
    // `N` is a const generic, so this bound is checked at the one place it
    // could go wrong rather than at every call site.
    let cap = if N > i32::MAX as usize {
        i32::MAX
    } else {
        N as i32
    };
    let full = call(out.buf.as_mut_ptr() as i32, cap);
    if full < 0 {
        return out;
    }
    out.present = true;
    out.full = full as usize;
    // The host writes `min(cap, full)` bytes and cuts at a byte, so a value
    // that did not fit can end mid-character. Trimming that is this side's
    // job — half a code point is not a shorter string, it is bytes nothing
    // can turn back into one.
    out.len = if out.full <= N {
        out.full
    } else {
        whole_characters(&out.buf, N)
    };
    out
}

/// How much of `buf[..end]` is complete UTF-8, assuming everything before the
/// cut was.
///
/// Walks back over continuation bytes to find where the last sequence begins,
/// then asks whether the sequence its leader declares actually fits. Bounded
/// by UTF-8's four bytes, so this is a handful of comparisons and not a scan.
fn whole_characters(buf: &[u8], end: usize) -> usize {
    // The first byte at or before the cut that is not a continuation. Note
    // that the loop can run zero times: a cut landing directly after a
    // *leader* has no continuation bytes to walk over and is still an
    // incomplete character, which is the case worth being careful about.
    let mut at = end;
    while at > 0 && (buf[at - 1] & 0b1100_0000) == 0b1000_0000 {
        at -= 1;
    }
    // Nothing but continuation bytes: not UTF-8 at all, and there is nothing
    // here to salvage by trimming.
    if at == 0 {
        return end;
    }
    let leader_at = at - 1;
    let needed = match buf[leader_at] {
        b if b >> 5 == 0b110 => 2,
        b if b >> 4 == 0b1110 => 3,
        b if b >> 3 == 0b11110 => 4,
        // An ASCII byte, or a leader this encoding does not define: either
        // way nothing after it belongs to it.
        _ => 1,
    };
    if leader_at + needed <= end {
        end
    } else {
        leader_at
    }
}

/// What a plugin declares about itself, during `oxi_init` and only then.
///
/// A builder rather than a return value because it is a list that will grow,
/// and because what a user is shown before enabling a plugin should be one
/// readable block in its source.
pub struct Setup {
    _private: (),
}

impl Setup {
    /// Only [`plugin!`] makes one, and only inside `oxi_init`.
    #[must_use]
    #[doc(hidden)]
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// The name a user sees beside this plugin's settings.
    pub fn name(&mut self, name: &str) {
        unsafe { raw::set_name(name.as_ptr() as i32, name.len() as i32) };
    }

    /// Which event kinds to be handed. Build it from `abi::kinds::bit`.
    pub fn subscribe(&mut self, mask: i64) {
        raw::subscribe(mask);
    }

    /// What this plugin may do. Build it from `abi::caps`.
    ///
    /// Asking for less is the whole value of asking at all: this is the
    /// sentence a user reads before deciding to run a file they downloaded.
    pub fn capabilities(&mut self, mask: i64) {
        raw::request_caps(mask);
    }
}

impl Default for Setup {
    fn default() -> Self {
        Self::new()
    }
}

// ---- commands ------------------------------------------------------------

/// Send a message into a chat. Needs `abi::caps::SEND`.
pub fn send_text(jid: &str, text: &str) -> Outcome {
    Outcome::of(unsafe {
        raw::send_text(
            jid.as_ptr() as i32,
            jid.len() as i32,
            text.as_ptr() as i32,
            text.len() as i32,
        )
    })
}

/// Send a message as a reply to `quoted_id`. Needs `abi::caps::SEND`.
pub fn send_reply(jid: &str, text: &str, quoted_id: &str) -> Outcome {
    Outcome::of(unsafe {
        raw::send_reply(
            jid.as_ptr() as i32,
            jid.len() as i32,
            text.as_ptr() as i32,
            text.len() as i32,
            quoted_id.as_ptr() as i32,
            quoted_id.len() as i32,
        )
    })
}

/// Mark a chat read through `message_id`, or as far as the daemon knows when
/// that is `None`. Needs `abi::caps::MARK_READ`.
pub fn mark_read(jid: &str, message_id: Option<&str>) -> Outcome {
    let id = message_id.unwrap_or("");
    Outcome::of(unsafe {
        raw::mark_read(
            jid.as_ptr() as i32,
            jid.len() as i32,
            id.as_ptr() as i32,
            id.len() as i32,
        )
    })
}

/// Show or clear a typing indicator in a chat. Needs `abi::caps::TYPING`.
pub fn typing(jid: &str, composing: bool) -> Outcome {
    Outcome::of(unsafe { raw::typing(jid.as_ptr() as i32, jid.len() as i32, i32::from(composing)) })
}

/// Publish this plugin's whole interface, replacing whatever it drew before.
///
/// Build `tree` with [`abi::ui::Writer`] over a buffer of your own. Needs
/// `abi::caps::UI`.
pub fn set_ui(tree: &[u8]) -> Outcome {
    Outcome::of(unsafe { raw::ui_set(tree.as_ptr() as i32, tree.len() as i32) })
}

/// Read a stored value. Needs `abi::caps::STORAGE`.
#[must_use]
pub fn get<const N: usize>(key: &str) -> Text<N> {
    read_into(|ptr, cap| unsafe { raw::kv_get(key.as_ptr() as i32, key.len() as i32, ptr, cap) })
}

/// Store a value, or remove it when `value` is empty. Needs
/// `abi::caps::STORAGE`.
pub fn set(key: &str, value: &str) -> Outcome {
    Outcome::of(unsafe {
        raw::kv_set(
            key.as_ptr() as i32,
            key.len() as i32,
            value.as_ptr() as i32,
            value.len() as i32,
        )
    })
}

/// Ask to be called back with `abi::kinds::TIMER` carrying `token`, once.
/// Needs `abi::caps::TIMERS`.
///
/// The host holds a floor under the delay: a plugin cannot re-arm its way
/// into spinning its own thread.
pub fn after(delay_ms: i64, token: i64) -> Outcome {
    Outcome::of(raw::timer_set(delay_ms, token))
}

/// Generate the three exports the host looks for.
///
/// A declarative macro rather than a derive, so this crate needs no
/// proc-macro dependency — and so what it generates is readable in the one
/// place it is written.
///
/// ```ignore
/// oxidezap_plugin::plugin!(init = setup, event = handle);
/// ```
///
/// `init` is `fn(&mut Setup)` and `event` is `fn(&Event)`.
#[macro_export]
macro_rules! plugin {
    (init = $init:path, event = $on_event:path $(,)?) => {
        /// The version this plugin was built against.
        ///
        /// A function, not a global: neither Rust nor TinyGo can emit an
        /// exported wasm global without post-processing the module.
        #[unsafe(no_mangle)]
        pub extern "C" fn oxi_abi_version() -> i32 {
            $crate::abi::VERSION
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn oxi_init() -> i32 {
            let mut setup = $crate::Setup::new();
            let f: fn(&mut $crate::Setup) = $init;
            f(&mut setup);
            0
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn oxi_on_event(kind: i32, handle: i32) -> i32 {
            let event = $crate::Event::new(kind, handle);
            let f: fn(&$crate::Event) = $on_event;
            f(&event);
            0
        }
    };
}

#[cfg(test)]
mod tests {
    use super::whole_characters;

    /// A value that fit is never trimmed, whatever it holds.
    #[test]
    fn a_clean_cut_keeps_everything() {
        let s = "ação".as_bytes();
        assert_eq!(whole_characters(s, s.len()), s.len());
        // "aç" — two characters, three bytes, and the cut is on a boundary.
        assert_eq!(whole_characters(s, 3), 3);
    }

    /// The case the host's byte-cut produces, and the reason this exists.
    #[test]
    fn a_split_character_is_dropped_whole() {
        // "ç" is two bytes; cutting after the first leaves a leader with
        // nothing behind it.
        let s = "ação".as_bytes();
        assert_eq!(whole_characters(s, 2), 1, "back to just the 'a'");
    }

    #[test]
    fn a_three_byte_character_is_dropped_at_either_cut() {
        // U+20AC EURO SIGN: e2 82 ac.
        let s = "a€".as_bytes();
        assert_eq!(whole_characters(s, 4), 4, "all of it");
        assert_eq!(whole_characters(s, 3), 1, "two of its three bytes");
        assert_eq!(whole_characters(s, 2), 1, "one of its three bytes");
    }

    #[test]
    fn a_four_byte_character_is_dropped_at_every_cut() {
        // U+1F600: f0 9f 98 80.
        let s = "x😀".as_bytes();
        assert_eq!(whole_characters(s, 5), 5);
        for cut in 2..5 {
            assert_eq!(whole_characters(s, cut), 1, "cut at {cut}");
        }
    }

    /// Bytes that are not UTF-8 at all have nothing to salvage, and this must
    /// answer rather than loop or index past the end.
    #[test]
    fn nothing_but_continuation_bytes_is_left_alone() {
        assert_eq!(whole_characters(&[0x80, 0x80, 0x80], 3), 3);
        assert_eq!(whole_characters(&[], 0), 0);
    }
}
