//! Write an oxidezap plugin in Rust.
//!
//! The raw ABI is four read functions, a handful of commands and three
//! exports; this crate is the thin, typed layer over it and nothing more.
//! There is no runtime here, no allocator, no `std` — a plugin built on this
//! carries the bytes of its own logic and almost nothing else, which is the
//! entire point of the ABI it wraps.
//!
//! ```ignore
//! use oxidezap_plugin::{abi, fields, plugin, Caps, Declared, Event, Kinds, Setup, send_text};
//!
//! plugin!(init = setup, event = handle);
//!
//! // Each of these is said once, and the type is what says so: a second
//! // `name` is a method that is not there rather than a refusal at load.
//! fn setup(p: Setup) -> impl Declared {
//!     p.name("Autoreply")
//!         .subscribe(Kinds::MESSAGE)
//!         .capabilities(Caps::SEND)
//! }
//!
//! fn handle(ev: &Event) {
//!     if ev.kind() != abi::kinds::MESSAGE || ev.flag(fields::FROM_ME) {
//!         return;
//!     }
//!     // No size at the call site: the field carries the room it needs.
//!     if ev.text(fields::TEXT).as_str().contains("ping") {
//!         // `whole`, because a JID that did not fit is somebody else.
//!         if let Some(chat) = ev.text(fields::CHAT_JID).whole() {
//!             send_text(chat, "pong");
//!         }
//!     }
//! }
//! ```
//!
//! Handlers are testable without a daemon: the `testing` feature answers the
//! imports from a table your test owns. See [`testing`].
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
// The test host records what a plugin asked for, which means a heap and a
// thread-local. Never in a plugin's own build: a `.wasm` links the real
// imports and carries neither.
#[cfg(all(not(target_arch = "wasm32"), feature = "testing"))]
extern crate std;

pub use oxidezap_plugin_abi as abi;

mod raw;

#[cfg(all(not(target_arch = "wasm32"), feature = "testing"))]
pub mod testing;

pub use raw::{level, log, now_ms};

// ---- typed masks ---------------------------------------------------------

/// What a plugin may do, as a value rather than as an integer.
///
/// `subscribe` and `capabilities` both used to take an `i64`, so
/// `p.subscribe(caps::SEND)` compiled and was discovered — if at all — at
/// runtime, as a plugin hearing about the wrong events. These are two
/// different sets and now two different types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps(i64);

impl Caps {
    /// Nothing at all, which is what a plugin that only watches asks for.
    pub const NONE: Self = Self(0);
    /// Send messages, as this account.
    pub const SEND: Self = Self(abi::caps::SEND);
    /// Mark somebody's messages read.
    pub const MARK_READ: Self = Self(abi::caps::MARK_READ);
    /// Show a typing indicator.
    pub const TYPING: Self = Self(abi::caps::TYPING);
    /// Draw buttons and settings.
    pub const UI: Self = Self(abi::caps::UI);
    /// Keep its own settings across restarts.
    pub const STORAGE: Self = Self(abi::caps::STORAGE);
    /// Wake itself on a timer.
    pub const TIMERS: Self = Self(abi::caps::TIMERS);

    /// The mask the ABI carries.
    #[must_use]
    pub const fn bits(self) -> i64 {
        self.0
    }
}

impl core::ops::BitOr for Caps {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Which events a plugin is handed.
///
/// A set of kinds, not a set of capabilities: asking for kinds it never looks
/// at makes the daemon convert and queue an account's whole traffic for
/// nothing, and receipts and presence are most of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kinds(i64);

impl Kinds {
    pub const NONE: Self = Self(0);
    pub const MESSAGE: Self = Self(abi::kinds::bit(abi::kinds::MESSAGE));
    pub const CONNECTION: Self = Self(abi::kinds::bit(abi::kinds::CONNECTION));
    pub const RECEIPT: Self = Self(abi::kinds::bit(abi::kinds::RECEIPT));
    pub const REACTION: Self = Self(abi::kinds::bit(abi::kinds::REACTION));
    pub const PRESENCE: Self = Self(abi::kinds::bit(abi::kinds::PRESENCE));
    pub const CALL: Self = Self(abi::kinds::bit(abi::kinds::CALL));
    /// Somebody used one of this plugin's own widgets. Delivered whatever
    /// this asks for — a plugin that draws is told when its controls are
    /// used — but naming it here is what makes a handler's `match` honest.
    pub const UI_ACTION: Self = Self(abi::kinds::bit(abi::kinds::UI_ACTION));
    /// A timer this plugin armed. Delivered whatever this asks for, for the
    /// same reason.
    pub const TIMER: Self = Self(abi::kinds::bit(abi::kinds::TIMER));

    #[must_use]
    pub const fn bits(self) -> i64 {
        self.0
    }
}

impl core::ops::BitOr for Kinds {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

// ---- fields, with the room they need -------------------------------------

/// A field, and how much room reading it wants.
///
/// The size used to be chosen at every call site — `ev.text::<128>(CHAT_JID)`
/// — which put a correctness question in the caller's hands each time: a JID
/// that did not fit is not a shorter JID, it is somebody else. Carrying it on
/// the field means the number is decided once, next to what it describes, and
/// inferred everywhere it is read.
///
/// It is a recommendation rather than a rule: [`sized`](Self::sized) asks for
/// a different one where a plugin knows better.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field<const N: usize>(i32);

impl<const N: usize> Field<N> {
    /// A field this table does not name, at a size of your choosing.
    #[must_use]
    pub const fn new(id: i32) -> Self {
        Self(id)
    }

    /// The same field, read into a buffer of a different size.
    #[must_use]
    pub const fn sized<const M: usize>(self) -> Field<M> {
        Field(self.0)
    }

    /// The number the ABI carries.
    #[must_use]
    pub const fn id(self) -> i32 {
        self.0
    }
}

/// Every field an event can carry, with the room a read of it wants.
///
/// The sizes are what the thing actually is: a JID and a message id are
/// bounded by the protocol, a message body is not and gets a generous page,
/// and a widget's id is as long as the plugin's own names.
pub mod fields {
    use super::Field;
    use oxidezap_plugin_abi as abi;

    /// The chat an event happened in.
    pub const CHAT_JID: Field<128> = Field::new(abi::fields::CHAT_JID);
    /// Whether that chat is a group.
    pub const IS_GROUP: Field<1> = Field::new(abi::fields::IS_GROUP);
    /// The message's id, which a reply quotes.
    pub const MESSAGE_ID: Field<128> = Field::new(abi::fields::MESSAGE_ID);
    /// What it says.
    pub const TEXT: Field<1024> = Field::new(abi::fields::TEXT);
    /// Whether this account wrote it.
    pub const FROM_ME: Field<1> = Field::new(abi::fields::FROM_ME);
    /// When, in milliseconds.
    pub const TIMESTAMP_MS: Field<1> = Field::new(abi::fields::TIMESTAMP_MS);
    /// Who wrote it, in a group.
    pub const SENDER_JID: Field<128> = Field::new(abi::fields::SENDER_JID);
    /// What they are called.
    pub const SENDER_NAME: Field<128> = Field::new(abi::fields::SENDER_NAME);
    /// Whether its author has taken it back.
    pub const REVOKED: Field<1> = Field::new(abi::fields::REVOKED);
    /// What kind of attachment it carries, if any.
    pub const MEDIA_KIND: Field<32> = Field::new(abi::fields::MEDIA_KIND);
    /// The message this one quotes.
    pub const QUOTED_ID: Field<128> = Field::new(abi::fields::QUOTED_ID);

    /// The connection's state.
    pub const CONNECTION_STATE: Field<32> = Field::new(abi::fields::CONNECTION_STATE);
    /// Why it changed.
    pub const REASON: Field<256> = Field::new(abi::fields::REASON);

    /// Whether a receipt is a delivery or a read.
    pub const RECEIPT_KIND: Field<32> = Field::new(abi::fields::RECEIPT_KIND);
    /// The messages it covers. A list: read it with `Event::count` and
    /// `Event::at`.
    pub const MESSAGE_IDS: Field<128> = Field::new(abi::fields::MESSAGE_IDS);

    /// A reaction's emoji. Empty when the reaction was removed.
    pub const EMOJI: Field<32> = Field::new(abi::fields::EMOJI);
    /// Whether somebody is typing, rather than having stopped.
    pub const COMPOSING: Field<1> = Field::new(abi::fields::COMPOSING);

    /// The call this is about.
    pub const CALL_ID: Field<128> = Field::new(abi::fields::CALL_ID);
    /// What happened to it.
    pub const CALL_EVENT: Field<32> = Field::new(abi::fields::CALL_EVENT);
    /// Whether it carries video.
    pub const CALL_IS_VIDEO: Field<1> = Field::new(abi::fields::CALL_IS_VIDEO);
    /// Who is on the other end.
    pub const PEER_JID: Field<128> = Field::new(abi::fields::PEER_JID);

    /// Which of this plugin's widgets was used.
    pub const ACTION_ID: Field<64> = Field::new(abi::fields::ACTION_ID);
    /// What it now holds. A setting is a keyword or a sentence.
    pub const ACTION_VALUE: Field<256> = Field::new(abi::fields::ACTION_VALUE);

    /// The token a timer was armed with.
    pub const TIMER_TOKEN: Field<1> = Field::new(abi::fields::TIMER_TOKEN);
}

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
    /// A value this side already has, as the same type a read returns.
    ///
    /// For a default: `kv::text` hands back either what was stored or this,
    /// and a caller should not have to tell the two apart.
    #[must_use]
    pub fn of(text: &str) -> Self {
        let mut out = Self::absent();
        out.present = true;
        out.full = text.len();
        out.len = if text.len() <= N {
            text.len()
        } else {
            whole_characters(text.as_bytes(), N)
        };
        out.buf[..out.len].copy_from_slice(&text.as_bytes()[..out.len]);
        out
    }

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

    /// Whether the host answered with a value rather than `ABSENT`.
    ///
    /// Not a way to tell a present empty string from an absent one: the ABI's
    /// absence rule is that a field's absence reads back as its default, and
    /// a string's default *is* empty — so the host reports both the same way,
    /// deliberately, which is what makes adding a field a non-event for a
    /// plugin built against an older table. This says only that something was
    /// there to copy; where a plugin needs "cleared" told apart from "not
    /// carried", the event has to say so in a field of its own.
    #[must_use]
    pub fn present(&self) -> bool {
        self.present
    }

    /// The value, but only when all of it fit.
    ///
    /// The companion to [`as_str`](Self::as_str), which hands back what was
    /// read whether or not that is everything. Which of the two to use is a
    /// question about the field: a JID that did not fit is not a shorter JID,
    /// it is somebody else, while a label that did not fit is still a label.
    #[must_use]
    pub fn whole(&self) -> Option<&str> {
        if self.complete() {
            Some(self.as_str())
        } else {
            None
        }
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
    pub fn text<const N: usize>(&self, field: Field<N>) -> Text<N> {
        let field = field.id();
        read_into(|ptr, cap| unsafe { raw::field_str(self.handle, field, ptr, cap) })
    }

    /// Read an integer field. Absent reads back as `0`, by the ABI's absence
    /// rule.
    #[must_use]
    pub fn int<const N: usize>(&self, field: Field<N>) -> i64 {
        let field = field.id();
        raw::field_i64(self.handle, field)
    }

    /// Read a boolean field. Absent reads back as `false`.
    #[must_use]
    pub fn flag<const N: usize>(&self, field: Field<N>) -> bool {
        raw::field_i64(self.handle, field.id()) != 0
    }

    /// How many elements a repeated field has.
    #[must_use]
    pub fn count<const N: usize>(&self, field: Field<N>) -> usize {
        let field = field.id();
        let n = raw::field_len(self.handle, field);
        if n < 0 { 0 } else { n as usize }
    }

    /// One element of a repeated field, as a string.
    #[must_use]
    pub fn at<const N: usize>(&self, field: Field<N>, index: usize) -> Text<N> {
        let field = field.id();
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
fn read_into<const N: usize>(mut call: impl FnMut(raw::Ptr, i32) -> i32) -> Text<N> {
    let mut out = Text::<N>::absent();
    // `N` is a const generic, so this bound is checked at the one place it
    // could go wrong rather than at every call site.
    let cap = if N > i32::MAX as usize {
        i32::MAX
    } else {
        N as i32
    };
    let full = call(raw::into(&mut out.buf), cap);
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
/// A builder that *consumes* itself, and whose methods exist only while the
/// thing they declare is still undeclared: calling `name` twice is not a
/// runtime refusal, it is a method that is not there. That matters because
/// these imports answer nothing — the host records a second declaration and
/// the loader refuses the module, which is a plugin that does not run and an
/// author reading a log line to find out why. The compiler is a better place
/// to be told.
///
/// ```ignore
/// fn setup(p: Setup) -> impl Declared {
///     p.name("Auto-reply")
///         .subscribe(Kinds::MESSAGE)
///         .capabilities(Caps::SEND | Caps::UI | Caps::STORAGE)
/// }
/// ```
pub struct Declaring<const NAMED: bool, const SUBSCRIBED: bool, const ASKED: bool> {
    _private: (),
}

/// What [`plugin!`] hands the init function: everything still to be said.
pub type Setup = Declaring<false, false, false>;

/// The end of a declaration, whatever was declared.
///
/// Implemented for every state, so an init function returns `impl Declared`
/// and says nothing about which of the three it used.
pub trait Declared {}

impl<const N: bool, const S: bool, const A: bool> Declared for Declaring<N, S, A> {}

impl Declaring<false, false, false> {
    /// Only [`plugin!`] makes one, and only inside `oxi_init`.
    #[must_use]
    #[doc(hidden)]
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for Declaring<false, false, false> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const S: bool, const A: bool> Declaring<false, S, A> {
    /// The name a user sees beside this plugin's settings.
    #[must_use]
    pub fn name(self, name: &str) -> Declaring<true, S, A> {
        unsafe { raw::set_name(raw::at(name.as_bytes()), name.len() as i32) };
        Declaring { _private: () }
    }
}

impl<const N: bool, const A: bool> Declaring<N, false, A> {
    /// Which event kinds to be handed.
    ///
    /// One call, so build the set with `|`: a second mask would replace the
    /// first rather than add to it, which is why there is no second call.
    #[must_use]
    pub fn subscribe(self, kinds: Kinds) -> Declaring<N, true, A> {
        raw::subscribe(kinds.bits());
        Declaring { _private: () }
    }
}

impl<const N: bool, const S: bool> Declaring<N, S, false> {
    /// What this plugin may do.
    ///
    /// Asking for less is the whole value of asking at all: this is the
    /// sentence a user reads before deciding to run a file they downloaded.
    #[must_use]
    pub fn capabilities(self, caps: Caps) -> Declaring<N, S, true> {
        raw::request_caps(caps.bits());
        Declaring { _private: () }
    }
}

// ---- commands ------------------------------------------------------------

/// Send a message into a chat. Needs `abi::caps::SEND`.
pub fn send_text(jid: &str, text: &str) -> Outcome {
    Outcome::of(unsafe {
        raw::send_text(
            raw::at(jid.as_bytes()),
            jid.len() as i32,
            raw::at(text.as_bytes()),
            text.len() as i32,
        )
    })
}

/// Send a message as a reply to `quoted_id`. Needs `abi::caps::SEND`.
pub fn send_reply(jid: &str, text: &str, quoted_id: &str) -> Outcome {
    Outcome::of(unsafe {
        raw::send_reply(
            raw::at(jid.as_bytes()),
            jid.len() as i32,
            raw::at(text.as_bytes()),
            text.len() as i32,
            raw::at(quoted_id.as_bytes()),
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
            raw::at(jid.as_bytes()),
            jid.len() as i32,
            raw::at(id.as_bytes()),
            id.len() as i32,
        )
    })
}

/// Show or clear a typing indicator in a chat. Needs `abi::caps::TYPING`.
pub fn typing(jid: &str, composing: bool) -> Outcome {
    Outcome::of(unsafe {
        raw::typing(
            raw::at(jid.as_bytes()),
            jid.len() as i32,
            i32::from(composing),
        )
    })
}

/// Publish this plugin's whole interface, replacing whatever it drew before.
///
/// Build `tree` with [`abi::ui::Writer`] over a buffer of your own. Needs
/// `abi::caps::UI`.
pub fn set_ui(tree: &[u8]) -> Outcome {
    Outcome::of(unsafe { raw::ui_set(raw::at(tree), tree.len() as i32) })
}

/// Read a stored value. Needs `abi::caps::STORAGE`.
#[must_use]
pub fn get<const N: usize>(key: &str) -> Text<N> {
    read_into(|ptr, cap| unsafe {
        raw::kv_get(raw::at(key.as_bytes()), key.len() as i32, ptr, cap)
    })
}

/// Store a value, or remove it when `value` is empty. Needs
/// `abi::caps::STORAGE`.
pub fn set(key: &str, value: &str) -> Outcome {
    Outcome::of(unsafe {
        raw::kv_set(
            raw::at(key.as_bytes()),
            key.len() as i32,
            raw::at(value.as_bytes()),
            value.len() as i32,
        )
    })
}

/// A plugin's own small store, in the shapes a plugin actually keeps.
///
/// Values are strings on the wire, because that is what the ABI carries. The
/// `"1"`/`"0"` pair a toggle arrives as is this module's business rather than
/// every plugin's, spelled once here instead of at each call site.
pub mod kv {
    use super::{Outcome, Text, get, set};

    /// A stored flag. Absent reads back as `false`, by the absence rule.
    #[must_use]
    pub fn flag(key: &str) -> bool {
        get::<2>(key).as_str() == "1"
    }

    /// Store a flag. Needs [`Caps::STORAGE`](super::Caps::STORAGE).
    pub fn set_flag(key: &str, on: bool) -> Outcome {
        set(key, if on { "1" } else { "0" })
    }

    /// A stored setting, or `fallback` when nothing is stored.
    ///
    /// One size for the read and for what a plugin writes back, because two
    /// would mean a value kept whole and matched on its first `N` bytes.
    #[must_use]
    pub fn text<const N: usize>(key: &str, fallback: &str) -> Text<N> {
        let stored = get::<N>(key);
        if stored.is_empty() {
            Text::of(fallback)
        } else {
            stored
        }
    }
}

/// Building the small tree a plugin publishes.
///
/// The encoder underneath takes `begin`/`end` pairs and a buffer whose size
/// the caller picks: an unbalanced pair is a malformed tree the daemon
/// refuses, and a slot on a child is one it refuses differently. Here a
/// section takes a closure, so there is no `end` to forget, and a widget
/// inside one has no slot to pass because children do not have one.
pub mod ui {
    use super::{Outcome, abi, set_ui};

    pub use oxidezap_plugin_abi::ui::slot;

    /// A plugin's whole interface, under construction.
    pub struct Canvas<'a> {
        writer: abi::ui::Writer<'a>,
    }

    /// The inside of a section, where widgets have no slot of their own.
    pub struct Group<'a, 'b> {
        canvas: &'b mut Canvas<'a>,
    }

    /// Build a tree in `N` bytes of stack and publish it. Needs
    /// [`Caps::UI`](super::Caps::UI).
    ///
    /// Whole every time, never a delta: the daemon compares what arrives
    /// against what it holds and publishes nothing when they match, so
    /// redrawing on every change costs a comparison rather than a frame.
    pub fn publish<const N: usize>(build: impl FnOnce(&mut Canvas)) -> Outcome {
        let mut buf = [0u8; N];
        let mut canvas = Canvas {
            writer: abi::ui::Writer::new(&mut buf),
        };
        build(&mut canvas);
        let Ok(len) = canvas.writer.finish() else {
            // The tree outgrew the buffer this call gave it, which is the
            // plugin's own number and its own mistake to hear about.
            return Outcome::Invalid;
        };
        set_ui(&buf[..len])
    }

    impl Canvas<'_> {
        /// A button on its own, in a slot.
        pub fn button(&mut self, slot: u8, id: &str, label: &str) {
            self.writer.leaf(
                abi::ui::kind::BUTTON,
                slot,
                abi::ui::flags::ENABLED,
                id,
                label,
                "",
            );
        }

        /// A group of widgets under a heading.
        ///
        /// Closed for you: the `end` that used to be the caller's to remember
        /// is this function returning.
        pub fn section(&mut self, slot: u8, label: &str, build: impl FnOnce(&mut Group)) {
            self.writer.begin(
                abi::ui::kind::SECTION,
                slot,
                abi::ui::flags::ENABLED,
                "",
                label,
                "",
            );
            build(&mut Group { canvas: self });
            self.writer.end();
        }
    }

    impl Group<'_, '_> {
        /// A switch. `on` is what it shows, `live` whether it may be used.
        pub fn toggle(&mut self, id: &str, label: &str, on: bool, live: bool) {
            let flags = if live { abi::ui::flags::ENABLED } else { 0 }
                | if on { abi::ui::flags::CHECKED } else { 0 };
            self.canvas.writer.leaf(
                abi::ui::kind::TOGGLE,
                abi::ui::slot::NONE,
                flags,
                id,
                label,
                if on { "1" } else { "0" },
            );
        }

        /// A box somebody types in. Its commit arrives as an action.
        pub fn field(&mut self, id: &str, label: &str, value: &str, live: bool) {
            let flags = if live { abi::ui::flags::ENABLED } else { 0 };
            self.canvas.writer.leaf(
                abi::ui::kind::TEXT_FIELD,
                abi::ui::slot::NONE,
                flags,
                id,
                label,
                value,
            );
        }

        /// A line of text. Carries no id, because nothing can be done to it.
        pub fn label(&mut self, text: &str) {
            self.canvas
                .writer
                .leaf(abi::ui::kind::LABEL, abi::ui::slot::NONE, 0, "", text, "");
        }

        /// A button inside a section.
        pub fn button(&mut self, id: &str, label: &str) {
            self.canvas.writer.leaf(
                abi::ui::kind::BUTTON,
                abi::ui::slot::NONE,
                abi::ui::flags::ENABLED,
                id,
                label,
                "",
            );
        }
    }
}

/// Ask to be called back with `abi::kinds::TIMER` carrying `token`, once.
/// Needs `abi::caps::TIMERS`.
///
/// The host holds a floor under the delay: a plugin cannot re-arm its way
/// into spinning its own thread. And a ceiling — a week — because past it a
/// delay is not a time any clock can hold, so an arithmetic mistake here is
/// [`Outcome::Refused`] rather than a timer that never fires.
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
/// `init` is `fn(Setup) -> impl Declared` and `event` is `fn(&Event)`.
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
            // Generic over what the declaration ended as, because the type
            // says which of the three things were said and the macro has no
            // business caring.
            fn declare<D: $crate::Declared>(f: fn($crate::Setup) -> D) -> i32 {
                let _ = f($crate::Setup::new());
                0
            }
            declare($init)
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
    use super::{Caps, Field, Kinds, Text, fields, whole_characters};

    /// Two sets, two types. `subscribe(Caps::SEND)` used to compile — both
    /// took an `i64` — and a plugin discovered it by never hearing about the
    /// events it meant to ask for.
    #[test]
    fn a_capability_is_not_an_event_kind() {
        assert_eq!(
            (Caps::SEND | Caps::UI).bits(),
            oxidezap_plugin_abi::caps::SEND | oxidezap_plugin_abi::caps::UI
        );
        assert_eq!(
            Kinds::MESSAGE.bits(),
            oxidezap_plugin_abi::kinds::bit(oxidezap_plugin_abi::kinds::MESSAGE)
        );
        // The two masks are the same integer here, and that is the point:
        // nothing but the type tells them apart.
        assert_eq!(Caps::SEND.bits(), Kinds::NONE.bits() | 1);
    }

    /// The size travels with the field, so a read does not pick one.
    #[test]
    fn a_field_carries_the_room_it_needs() {
        let jid: Field<128> = fields::CHAT_JID;
        assert_eq!(jid.id(), oxidezap_plugin_abi::fields::CHAT_JID);
        // And a plugin that knows better says so, without changing which
        // field it is reading.
        let bigger: Field<4096> = fields::TEXT.sized::<4096>();
        assert_eq!(bigger.id(), oxidezap_plugin_abi::fields::TEXT);
    }

    /// `whole` is the question `as_str` does not ask.
    #[test]
    fn a_value_that_did_not_fit_is_not_a_shorter_value() {
        let fits: Text<16> = Text::of("5511999@s.wa");
        assert_eq!(fits.whole(), Some("5511999@s.wa"));

        let cut: Text<4> = Text::of("5511999@s.whatsapp.net");
        assert_eq!(
            cut.whole(),
            None,
            "somebody else's number, not a shorter one"
        );
        assert_eq!(cut.as_str(), "5511", "still readable, for a label");
    }

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
