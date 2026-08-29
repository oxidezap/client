//! The wasm ABI: what a plugin exports, what the host offers, and the numbers
//! both sides agree on.
//!
//! This crate is the single place either side may learn a constant from. It
//! is compiled into the daemon *and* into every plugin, including ones built
//! for `wasm32-unknown-unknown` with no allocator, which is why it is
//! `no_std`, has no dependencies, and never allocates outside the `std`
//! feature.
//!
//! # Shape
//!
//! A plugin exports three things and nothing else:
//!
//! ```text
//! oxi_abi_version()      -> i32  // must equal `VERSION`
//! oxi_init()             -> i32  // 0 on success; declares itself by calling back
//! oxi_on_event(kind, ev) -> i32  // 0 on success
//! ```
//!
//! A function rather than an exported wasm global, deliberately. Neither
//! Rust nor TinyGo can emit a global export without hand-editing the module
//! afterwards, and an ABI whose very first requirement needs a post-processing
//! step is one nobody implements correctly.
//!
//! Everything else flows through imports from the `oxidezap` module, listed
//! in [`imports`]. There is no `oxi_alloc`: the host never allocates inside a
//! plugin. Data is *pulled* — the plugin hands over a buffer it already owns
//! and the host writes into it — which is what keeps the ABI free of any
//! coupling to the guest's allocator, and what lets a language with a garbage
//! collector implement it without ceremony.
//!
//! # Absence
//!
//! One rule governs every read: **a field's absence reads back as that
//! field's default.** An i64 that is not there is `0`; a string that is not
//! there is empty, reported as [`ABSENT`]. This is the same contract the
//! socket protocol already holds itself to with `#[serde(default,
//! skip_serializing_if …)]`, and it is what makes adding a field a
//! non-event: a plugin built against an older [`fields`] table simply never
//! asks, and one built against a newer table asks and is told nothing is
//! there.

#![cfg_attr(not(feature = "std"), no_std)]

pub mod ui;

/// The ABI revision both sides must agree on.
///
/// A plugin exports this as `oxi_abi_version()` and the host refuses one that
/// disagrees before calling `oxi_init` and before handing it any event — the
/// same idiom the socket uses in its hello, and for the same reason: a host
/// that cannot understand a plugin's calls must not run its logic, and a
/// plugin that cannot understand the host's events must not be handed any.
///
/// The module is instantiated first, because reading an exported function
/// means calling one. That is bounded by the same fuel budget everything else
/// is, so a module whose *setup* misbehaves is caught there rather than
/// getting to run on the strength of a version it never had to state.
///
/// Bumped only for a change that breaks an existing plugin. Adding a field
/// constant, an event kind or a capability does not, by the absence rule in
/// the module docs.
pub const VERSION: i32 = 1;

/// The import module every host function lives in.
pub const MODULE: &str = "oxidezap";

/// The names of the three things a plugin exports.
pub mod exports {
    /// `fn() -> i32`, compared against [`super::VERSION`] before `oxi_init`.
    pub const ABI_VERSION: &str = "oxi_abi_version";
    /// `fn() -> i32`. Called once. Returns 0, or the plugin is refused.
    ///
    /// A plugin declares itself from inside this call, rather than through
    /// this function's return value, by calling [`super::imports::SUBSCRIBE`],
    /// [`super::imports::REQUEST_CAPS`] and [`super::imports::SET_NAME`]. A
    /// declaration made of host calls can grow without changing a signature,
    /// and the host can refuse one made at any other time.
    pub const INIT: &str = "oxi_init";
    /// `fn(kind: i32, ev: i32) -> i32`. The only entry point after init.
    pub const ON_EVENT: &str = "oxi_on_event";
    /// The linear memory the host reads and writes through. Wasm names the
    /// default one `memory`; a plugin that exports none can be handed nothing
    /// and is refused at load.
    pub const MEMORY: &str = "memory";
}

/// The names of every host function, in the [`MODULE`] namespace.
pub mod imports {
    // Declaration. Callable only from inside `oxi_init`; the host answers
    // `ERR_STATE` anywhere else, so a plugin cannot quietly widen what it may
    // do after the user has been shown what it asked for.

    /// `fn(mask: i64)` — which event kinds to deliver. See [`super::kinds`].
    pub const SUBSCRIBE: &str = "oxi_subscribe";
    /// `fn(mask: i64)` — which commands this plugin may issue. See
    /// [`super::caps`].
    pub const REQUEST_CAPS: &str = "oxi_request_caps";
    /// `fn(ptr: i32, len: i32) -> i32` — the display name a user sees.
    pub const SET_NAME: &str = "oxi_set_name";

    // Reading the event being handled. Every one of these takes the `ev`
    // handle `oxi_on_event` was given; handles are arena-scoped and every one
    // becomes invalid when that call returns.

    /// `fn(ev: i32, field: i32, ptr: i32, cap: i32) -> i32`
    ///
    /// Writes at most `cap` bytes and returns the value's *full* length —
    /// so a short buffer is detected by `n > cap`, and the plugin can size
    /// one and ask again. [`super::ABSENT`] when the field is not there,
    /// which is distinct from a present-but-empty string.
    pub const FIELD_STR: &str = "oxi_field_str";
    /// `fn(ev: i32, field: i32) -> i64` — `0` when absent, per the absence
    /// rule. Booleans travel here as `0` and `1`.
    pub const FIELD_I64: &str = "oxi_field_i64";
    /// `fn(ev: i32, field: i32) -> i32` — how many elements a repeated field
    /// has; `0` when absent.
    pub const FIELD_LEN: &str = "oxi_field_len";
    /// `fn(ev: i32, field: i32, index: i32) -> i32` — a child handle, or
    /// [`super::ABSENT`] when the index is past the end.
    pub const FIELD_AT: &str = "oxi_field_at";

    // Acting. One import per command rather than one `oxi_request` taking a
    // serialized `ClientRequest`: commands are few and stable, and this is
    // what spares a plugin from carrying an encoder. Each returns an
    // outcome from `super::outcome`.

    /// `fn(jid, jid_len, text, text_len) -> i32`
    pub const SEND_TEXT: &str = "oxi_send_text";
    /// `fn(jid, jid_len, text, text_len, quoted_id, quoted_id_len) -> i32`
    pub const SEND_REPLY: &str = "oxi_send_reply";
    /// `fn(jid, jid_len, message_id, message_id_len) -> i32`
    pub const MARK_READ: &str = "oxi_mark_read";
    /// `fn(jid, jid_len, composing: i32) -> i32`
    pub const TYPING: &str = "oxi_typing";
    /// `fn(ptr, len) -> i32` — this plugin's whole UI, encoded by
    /// [`super::ui`]. Replaces whatever it published before.
    pub const UI_SET: &str = "oxi_ui_set";
    /// `fn(key, key_len, ptr, cap) -> i32` — same short-buffer convention as
    /// [`FIELD_STR`].
    pub const KV_GET: &str = "oxi_kv_get";
    /// `fn(key, key_len, val, val_len) -> i32`. An empty value deletes.
    pub const KV_SET: &str = "oxi_kv_set";
    /// `fn(delay_ms: i64, token: i64) -> i32` — deliver
    /// [`super::kinds::TIMER`] carrying `token`, once, after `delay_ms`.
    /// The host holds a floor under the delay and a ceiling over it: a
    /// delay past the ceiling is refused rather than armed, because one at
    /// the far end of the `i64` is a deadline no monotonic clock can hold.
    pub const TIMER_SET: &str = "oxi_timer_set";

    // Free to everyone: neither reveals anything the plugin was not already
    // handed, so neither is behind a capability.

    /// `fn(level: i32, ptr: i32, len: i32)` — see [`super::log`].
    pub const LOG: &str = "oxi_log";
    /// `fn() -> i64` — Unix milliseconds.
    ///
    /// The only clock. A plugin has no other way to observe time passing,
    /// which is deliberate: a fine-grained one is a side channel, and nothing
    /// a plugin legitimately does needs better than this.
    pub const NOW_MS: &str = "oxi_now_ms";
}

/// Event kinds, as passed to `oxi_on_event` and as bit positions in the
/// subscription mask.
///
/// Deliberately coarser than the daemon's own `UiEvent`: a plugin asks for
/// "messages", not for each of the four events a message can produce. The
/// finer distinctions are fields on the event.
pub mod kinds {
    /// A message *arrived*, in either direction. `FROM_ME` says which.
    ///
    /// Arriving is the operative word, and both directions are real: one this
    /// account wrote on another device syncs in here with `FROM_ME` set,
    /// which is why an autoreply has to check it. What this does **not**
    /// carry is a send made through this daemon — by a window, or by a plugin
    /// itself. Those are not re-delivered: the session announces them as an
    /// id assignment rather than as a message, and synthesizing one would
    /// hand a plugin its own send twice once the same message came back
    /// through sync.
    pub const MESSAGE: i32 = 1;
    /// The connection to WhatsApp changed. `CONNECTION_STATE` says how.
    pub const CONNECTION: i32 = 2;
    /// A receipt landed on one or more messages.
    pub const RECEIPT: i32 = 3;
    /// Somebody reacted to a message.
    pub const REACTION: i32 = 4;
    /// Somebody started or stopped composing.
    pub const PRESENCE: i32 = 5;
    /// A call started, was answered, or ended. `CALL_EVENT` says which.
    pub const CALL: i32 = 6;
    /// Someone interacted with a widget this plugin published.
    ///
    /// Always delivered, whatever the subscription mask says: a plugin that
    /// drew a button is subscribed to that button by having drawn it, and a
    /// mask that could exclude it would only ever be a bug.
    pub const UI_ACTION: i32 = 7;
    /// A timer this plugin set has come due.
    ///
    /// Always delivered, for the same reason as [`UI_ACTION`].
    pub const TIMER: i32 = 8;

    /// One past the highest kind. The host refuses a mask with bits above it,
    /// which is how a plugin built against a newer ABI is caught early rather
    /// than by silently never hearing about the kind it wanted.
    pub const COUNT: i32 = 9;

    /// The bit that subscribes to `kind`.
    #[must_use]
    pub const fn bit(kind: i32) -> i64 {
        1i64 << kind
    }

    /// Whether `mask` asks for `kind`.
    #[must_use]
    pub const fn subscribed(mask: i64, kind: i32) -> bool {
        mask & bit(kind) != 0
    }

    /// The kinds a plugin gets whether it asked or not.
    #[must_use]
    pub const fn always_delivered(kind: i32) -> bool {
        kind == UI_ACTION || kind == TIMER
    }
}

/// What a plugin may *do*, as opposed to what it may see.
///
/// Separate from the subscription mask because the two answer different
/// questions and only one of them is worth showing a user before they enable
/// a downloaded file. "Reads your messages" and "sends messages as you" are
/// not the same sentence.
pub mod caps {
    /// [`super::imports::SEND_TEXT`] and [`super::imports::SEND_REPLY`].
    pub const SEND: i64 = 1 << 0;
    /// [`super::imports::MARK_READ`].
    pub const MARK_READ: i64 = 1 << 1;
    /// [`super::imports::TYPING`].
    pub const TYPING: i64 = 1 << 2;
    /// [`super::imports::UI_SET`], and so the right to be drawn.
    pub const UI: i64 = 1 << 3;
    /// [`super::imports::KV_GET`] and [`super::imports::KV_SET`]: state that
    /// outlives the daemon.
    pub const STORAGE: i64 = 1 << 4;
    /// [`super::imports::TIMER_SET`].
    pub const TIMERS: i64 = 1 << 5;

    /// Every capability this ABI defines. The host refuses a request with a
    /// bit outside it.
    pub const ALL: i64 = SEND | MARK_READ | TYPING | UI | STORAGE | TIMERS;

    /// The ones that act on the *account*, and so do not take effect until
    /// the user has agreed to them.
    ///
    /// The line is drawn at what a plugin can do that someone else would
    /// notice: send as you, clear your unread, tell a contact you are typing.
    /// The rest — drawing, its own settings file, its own timer — is confined
    /// to the plugin, and gating those would mean a plugin could not draw the
    /// panel explaining itself before it was allowed to draw anything, which
    /// would leave the user agreeing to a name and a bit-list.
    ///
    /// A plugin holds these only once approved, and holds them from before
    /// `oxi_init` runs rather than after: init is code the plugin chose too,
    /// and granting for the length of one call is granting.
    pub const NEEDS_APPROVAL: i64 = SEND | MARK_READ | TYPING;

    /// A short, user-facing name for one capability bit.
    ///
    /// Here rather than in the host because it is part of what the ABI
    /// *means*: a bit whose consequence cannot be stated in a phrase is one
    /// nobody can consent to.
    #[must_use]
    pub const fn describe(bit: i64) -> &'static str {
        match bit {
            SEND => "send messages",
            MARK_READ => "mark chats read",
            TYPING => "show a typing indicator",
            UI => "add buttons and settings",
            STORAGE => "keep its own settings",
            TIMERS => "run on a timer",
            _ => "unknown",
        }
    }

    /// Every defined bit, low to high, for a caller listing them.
    pub const EACH: [i64; 6] = [SEND, MARK_READ, TYPING, UI, STORAGE, TIMERS];
}

/// Field ids, shared across event kinds wherever the field means the same
/// thing.
///
/// A flat namespace of constants rather than an accessor per field, which is
/// the decision that keeps the import surface at four read functions no
/// matter how many fields exist. Ids are never reused: a retired field simply
/// stops being answered, and by the absence rule an old plugin asking for it
/// reads a default instead of another field's value.
pub mod fields {
    /// str — what *this* handle holds, for a handle that is one element of a
    /// repeated field rather than a whole event.
    ///
    /// Field zero is "itself", which is what makes `oxi_field_at` uniform: a
    /// list element is a handle like any other, read through the same four
    /// functions, whether it turns out to hold a string today or a structure
    /// tomorrow.
    pub const SELF: i32 = 0;

    // Shared: a message, a receipt, a reaction and a presence notice all name
    // a chat, so they all answer the same id for it.

    /// str — the conversation this is about.
    pub const CHAT_JID: i32 = 1;
    /// i64 — 1 for a group conversation.
    pub const IS_GROUP: i32 = 4;
    // 2 and 3 were the chat's name and unread count. They are not answered by
    // this ABI: the event a plugin is handed is the session's, and neither is
    // on it — filling them would mean the host consulting daemon state per
    // event for two fields nobody has asked for yet. The ids stay retired
    // rather than reused, so answering them later is an addition and not a
    // change.

    /// str — the message's id, server-assigned where it has one.
    pub const MESSAGE_ID: i32 = 10;
    /// str — the message's text, already flattened out of rich text.
    pub const TEXT: i32 = 11;
    /// i64 — 1 when this account wrote it.
    pub const FROM_ME: i32 = 12;
    /// i64 — Unix milliseconds.
    pub const TIMESTAMP_MS: i32 = 13;
    /// str — who wrote it, in a group. Empty in a one-to-one chat, where the
    /// sender is the chat.
    pub const SENDER_JID: i32 = 14;
    /// str — the sender's resolved display name. One name, chosen once; see
    /// `session/names.rs`.
    pub const SENDER_NAME: i32 = 15;
    /// i64 — 1 when the author has since deleted it. A revoked message is a
    /// row that still exists, so a plugin that treats one as ordinary text
    /// would answer "[Message deleted]".
    pub const REVOKED: i32 = 16;
    /// i64 — see [`media`]. `0` for a message that is only text.
    pub const MEDIA_KIND: i32 = 17;
    /// str — the id of the message this one replies to.
    pub const QUOTED_ID: i32 = 18;

    /// i64 — see [`connection`].
    pub const CONNECTION_STATE: i32 = 30;
    /// str — why, for a disconnect or a logout.
    pub const REASON: i32 = 31;

    /// i64 — see [`receipt`].
    pub const RECEIPT_KIND: i32 = 40;
    /// repeated str — which messages the receipt covers. Read with
    /// `oxi_field_len` and `oxi_field_at`.
    pub const MESSAGE_IDS: i32 = 41;

    /// str — the reaction itself. Empty when a reaction was removed.
    pub const EMOJI: i32 = 50;

    /// i64 — 1 while somebody is composing, 0 when they stopped.
    pub const COMPOSING: i32 = 60;

    /// str — which call.
    pub const CALL_ID: i32 = 70;
    /// i64 — see [`call`].
    pub const CALL_EVENT: i32 = 71;
    /// i64 — 1 when the call carries video.
    pub const CALL_IS_VIDEO: i32 = 72;
    /// str — who is on the other end.
    pub const PEER_JID: i32 = 73;

    /// str — the widget id the plugin gave the thing that was used.
    pub const ACTION_ID: i32 = 80;
    /// str — what it now holds: a toggle's new state as `1`/`0`, a text
    /// field's contents. Empty for a button, which carries no value.
    pub const ACTION_VALUE: i32 = 81;

    /// i64 — the token handed to `oxi_timer_set`.
    pub const TIMER_TOKEN: i32 = 90;

    /// Values of [`MEDIA_KIND`].
    pub mod media {
        pub const NONE: i64 = 0;
        pub const IMAGE: i64 = 1;
        pub const VIDEO: i64 = 2;
        pub const AUDIO: i64 = 3;
        pub const DOCUMENT: i64 = 4;
        pub const STICKER: i64 = 5;
    }

    /// Values of [`CONNECTION_STATE`].
    pub mod connection {
        pub const CONNECTING: i64 = 0;
        pub const PAIRING: i64 = 1;
        pub const SYNCING: i64 = 2;
        pub const CONNECTED: i64 = 3;
        pub const DISCONNECTED: i64 = 4;
        pub const LOGGED_OUT: i64 = 5;
    }

    /// Values of [`RECEIPT_KIND`].
    pub mod receipt {
        pub const DELIVERED: i64 = 0;
        pub const READ: i64 = 1;
        pub const PLAYED: i64 = 2;
    }

    /// Values of [`CALL_EVENT`].
    pub mod call {
        pub const INCOMING: i64 = 0;
        pub const OUTGOING: i64 = 1;
        pub const ANSWERED: i64 = 2;
        pub const ENDED: i64 = 3;
    }
}

/// Levels for [`imports::LOG`], matching the `log` crate's own order.
pub mod log {
    pub const ERROR: i32 = 1;
    pub const WARN: i32 = 2;
    pub const INFO: i32 = 3;
    pub const DEBUG: i32 = 4;
    pub const TRACE: i32 = 5;
}

/// What a command answered.
///
/// The daemon's own `CommandOutcome`, as an integer. A plugin learns this
/// where a socket front end does not, because the call is synchronous and so
/// needs no request id to correlate an answer with — see the note in
/// `AGENTS.md` about a front end being unable to say what went wrong.
pub mod outcome {
    /// The session took it. What the network makes of it arrives as an event.
    pub const ACCEPTED: i32 = 0;
    /// There was no session to carry it out.
    pub const NO_SESSION: i32 = -1;
    /// The session is there; the daemon will not do this as asked.
    pub const REFUSED: i32 = -2;
    /// The plugin did not declare the capability this needs.
    pub const DENIED: i32 = -3;
    /// The arguments did not make sense: a pointer outside memory, bytes that
    /// are not UTF-8, a length past what the host will accept.
    pub const INVALID: i32 = -4;
    /// Called at a moment this is not allowed — a declaration outside
    /// `oxi_init`.
    pub const STATE: i32 = -5;
}

/// A string field that is not there at all, distinct from one that is present
/// and empty.
pub const ABSENT: i32 = -1;

/// The longest string the host will read out of a plugin in one call.
///
/// Every `(ptr, len)` a plugin hands over is checked against this before the
/// host allocates anything to hold it, which is the point: a length is a
/// number the guest chose, and a host that allocates from it first has
/// already lost. Generous next to any real message and small next to a
/// plugin's whole memory.
pub const MAX_STR: usize = 64 * 1024;
