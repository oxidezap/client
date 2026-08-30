//! How one tab finds the tab that holds the account.
//!
//! There is still exactly one session per user, and it still lives in a
//! daemon. What changes here is that a second tab is no longer *refused*: on
//! the web the daemon is `daemon::embedded`, running in whichever tab won the
//! browser's lock, and a tab that lost it has the same thing a desktop window
//! has — a front end with no session, and a protocol to reach one over.
//!
//! So this is a fourth transport, and it is a transport rather than a special
//! case: [`crate::endpoint::tab`] is the client end, `daemon/listener/tab.rs`
//! is the server end, and the frames between them are the frames a socket
//! carries. The two places the platform split is allowed to live are exactly
//! the two it lives in.
//!
//! # Why a channel name and not a port
//!
//! The obvious carrier is a `MessagePort`, which is a pipe between two agents
//! and nothing else. It cannot be reached: a port is delivered by
//! *transferring* it, and `BroadcastChannel.postMessage` takes no transfer
//! list — there is nowhere to put one. A `SharedWorker` could hand ports out,
//! and is the shape this will eventually be, but it moves the session into
//! another agent, which is the expensive change /AGENTS.md describes and not
//! this one.
//!
//! What is reachable is a second `BroadcastChannel` under a name only the two
//! parties know. It is not a private channel — any script in this origin
//! could open the name if it had it — but neither is the first one, and a
//! script running in this origin is already the account's own code. What the
//! name buys is the thing that matters for cost: frames for one tab are not
//! delivered to every other tab, so a history load is cloned once per
//! connection rather than once per connection per tab.
//!
//! # Why the rendezvous is not the admission check
//!
//! It is not any kind of check. Everything here is same-origin by
//! construction — a `BroadcastChannel` is scoped to the origin, and so are
//! the database, the media and the Signal state it protects. What decides who
//! may hold the account is the lock in `daemon/claim/`, and what decides that
//! only one tab writes to the store is that the same lock is held for as long
//! as that tab lives.

use serde::{Deserialize, Serialize};

/// Reading and writing the messages one connection carries.
///
/// The rendezvous above is JSON, because it is three strings and a version
/// and it is read by tabs of other builds. A connection's own messages are
/// not: they carry a photo's bytes, and a structured clone of a `Uint8Array`
/// is one copy where JSON would be a base64 round trip through a string
/// twice the size. So they are plain objects, and this is the one place that
/// says what their fields are — both ends read it, and a field named twice in
/// two crates is a field that drifts.
#[cfg(target_family = "wasm")]
pub mod fields {
    use wasm_bindgen::JsCast as _;

    /// A string field, if it is there and is one.
    ///
    /// Every read goes through one of these: what arrives is whatever the
    /// other tab posted, and a missing or wrong-typed field is a message to
    /// ignore rather than a reason to panic inside a browser callback.
    #[must_use]
    pub fn string(data: &wasm_bindgen::JsValue, key: &str) -> Option<String> {
        js_sys::Reflect::get(data, &wasm_bindgen::JsValue::from_str(key))
            .ok()
            .and_then(|value| value.as_string())
    }

    /// A whole non-negative number field.
    #[must_use]
    pub fn number(data: &wasm_bindgen::JsValue, key: &str) -> Option<u64> {
        let value = js_sys::Reflect::get(data, &wasm_bindgen::JsValue::from_str(key)).ok()?;
        let number = value.as_f64()?;
        (number >= 0.0 && number.is_finite()).then_some(number as u64)
    }

    /// A flag, absent reading as false.
    #[must_use]
    pub fn flag(data: &wasm_bindgen::JsValue, key: &str) -> bool {
        js_sys::Reflect::get(data, &wasm_bindgen::JsValue::from_str(key))
            .ok()
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    }

    /// A payload, copied out of the array the browser cloned.
    #[must_use]
    pub fn bytes(data: &wasm_bindgen::JsValue, key: &str) -> Option<Vec<u8>> {
        js_sys::Reflect::get(data, &wasm_bindgen::JsValue::from_str(key))
            .ok()?
            .dyn_into::<js_sys::Uint8Array>()
            .ok()
            .map(|array| array.to_vec())
    }

    /// Write one field of a message being built.
    ///
    /// # Errors
    ///
    /// The engine refused the write, which is not something an object literal
    /// does — answered rather than unwrapped because the callers are browser
    /// callbacks, where a panic takes the tab's executor with it.
    pub fn set(
        object: &js_sys::Object,
        key: &str,
        value: &wasm_bindgen::JsValue,
    ) -> Result<(), wasm_bindgen::JsValue> {
        js_sys::Reflect::set(object, &wasm_bindgen::JsValue::from_str(key), value).map(|_| ())
    }
}

/// The channel every tab in this origin listens on.
///
/// One per origin, which is one per account, for the same reason the claim is.
pub const RENDEZVOUS: &str = "oxidezap-tabs";

/// The rendezvous protocol's version.
///
/// Carried on every message and checked on receipt, because two tabs in one
/// origin are not necessarily two of the same build: a page left open across
/// a deploy is the ordinary case, not the exotic one. A tab that does not
/// recognise the version says nothing rather than guessing — the follower
/// then finds no leader and starts its own attempt, which the lock refuses
/// or grants, and either way the account is safe.
pub const VERSION: u32 = 1;

/// How long a follower waits for a leader to answer before giving up on one.
///
/// Short, because the leader is in the same browser and an unanswered ask
/// means there is no leader rather than a slow one — the tab that would have
/// answered is gone, and what this tab does next is try to become the leader
/// itself.
pub const ANSWER_TIMEOUT_MS: i32 = 2_000;

/// What a tab says on [`RENDEZVOUS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "kebab-case")]
pub enum Rendezvous {
    /// A tab with no session is looking for the one that has it.
    ///
    /// The nonce is this ask's, not this tab's: a tab that asks twice — the
    /// leader closed between them — must not accept an answer to the first
    /// ask, which names a connection served by a daemon that has gone.
    Ask {
        /// The protocol version the asking tab speaks.
        v: u32,
        /// This ask's name.
        ask: String,
    },
    /// The tab holding the account, answering one ask.
    Serve {
        /// The protocol version the answering tab speaks.
        v: u32,
        /// The ask being answered.
        ask: String,
        /// The channel this connection's frames travel on.
        on: String,
    },
    /// A tab has taken the account and is ready to serve.
    ///
    /// Sent when a tab becomes the leader, and it is what makes a takeover
    /// quiet: the followers of a leader that closed have each queued for the
    /// lock, exactly one of them is granted it, and the rest are sitting on
    /// an ask nobody answered. Without this they would wait out
    /// [`ANSWER_TIMEOUT_MS`] and then try for a lock the new leader holds,
    /// which is a refusal and a retry rather than a reconnection.
    Leading {
        /// The protocol version the new leader speaks.
        v: u32,
    },
}

impl Rendezvous {
    /// This message as a line, or nothing if it cannot be written.
    ///
    /// Infallible in practice — every variant is three owned strings — and
    /// answered as an `Option` rather than unwrapped, because a panic in a
    /// broadcast handler takes the tab's whole executor with it.
    #[must_use]
    pub fn encode(&self) -> Option<String> {
        serde_json::to_string(self).ok()
    }

    /// A message this build understands, or nothing.
    ///
    /// Two ways to be nothing, and neither is worth a log line at warning
    /// level: something else in this origin is using the name, or a tab from
    /// another build is speaking a version this one does not have.
    #[must_use]
    pub fn decode(line: &str) -> Option<Self> {
        let message: Self = serde_json::from_str(line).ok()?;
        (message.version() == VERSION).then_some(message)
    }

    /// The version the sender said it speaks.
    #[must_use]
    pub const fn version(&self) -> u32 {
        match self {
            Self::Ask { v, .. } | Self::Serve { v, .. } | Self::Leading { v } => *v,
        }
    }
}

/// The channel one connection's frames travel on.
///
/// Derived from the ask rather than drawn separately, so there is one name to
/// keep unique and it is the one the ask already carries.
#[must_use]
pub fn channel_for(ask: &str) -> String {
    format!("oxidezap-tab-{ask}")
}

/// The lock a follower holds for as long as its connection is worth serving.
///
/// The leader queues for it, which is how a connection is closed: a
/// `BroadcastChannel` has no close event and a tab that is gone sends no
/// goodbye — the browser releases the lock, the leader's queued request is
/// granted, and the connection it was holding open is dropped. The same
/// mechanism as the account's own claim, asking a different question.
#[must_use]
pub fn liveness_lock_for(ask: &str) -> String {
    format!("oxidezap-tab-live-{ask}")
}

#[cfg(test)]
mod tests {
    use super::{Rendezvous, VERSION, channel_for, liveness_lock_for};

    /// Here rather than beside the wasm halves, for the reason a
    /// `wasm32`-only test is no test at all: it runs nowhere.
    #[test]
    fn every_message_round_trips() {
        for message in [
            Rendezvous::Ask {
                v: VERSION,
                ask: "abc".to_string(),
            },
            Rendezvous::Serve {
                v: VERSION,
                ask: "abc".to_string(),
                on: channel_for("abc"),
            },
            Rendezvous::Leading { v: VERSION },
        ] {
            let line = message.encode().expect("a message this small encodes");
            assert_eq!(Rendezvous::decode(&line), Some(message));
        }
    }

    #[test]
    fn another_builds_version_is_not_understood() {
        let line = Rendezvous::Leading { v: VERSION + 1 }
            .encode()
            .expect("encodes");
        assert_eq!(Rendezvous::decode(&line), None);
    }

    #[test]
    fn nothing_else_on_the_channel_is_read_as_a_message() {
        for line in ["", "{}", "null", "hello", r#"{"t":"ask"}"#] {
            assert_eq!(Rendezvous::decode(line), None, "{line}");
        }
    }

    /// The two names one connection is known by must not be the same name:
    /// the frames channel is opened by both parties, and the liveness lock is
    /// held by exactly one of them.
    #[test]
    fn a_connections_two_names_are_different() {
        assert_ne!(channel_for("abc"), liveness_lock_for("abc"));
    }
}
