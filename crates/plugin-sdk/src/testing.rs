//! A host to test a plugin's handlers against, without a daemon.
//!
//! A plugin's imports resolve against the sandbox, so its handlers can only
//! run inside `oxidezapd` — which means the only way to see whether one
//! answers the right message was to build for wasm32, copy a file and read a
//! log. That is a long way to go to find out that a keyword match is
//! case-sensitive.
//!
//! This is the other end of the same ABI: the off-target stubs, which
//! otherwise panic, answer from a table this module owns. It is not the
//! daemon — nothing here enforces fuel, capabilities or approval, and a
//! command is recorded rather than sent — so a handler that passes here can
//! still be refused there. What it does check is the half a plugin's author
//! writes: which field it reads, what it decides, and what it asks for.
//!
//! ```ignore
//! let mut host = Host::new();
//! host.store("enabled", "1");
//! host.deliver(Message::from("5511999@s.whatsapp.net", "ping"));
//! assert_eq!(host.sent(), [("5511999@s.whatsapp.net".into(), "pong".into())]);
//! ```

use std::borrow::ToOwned as _;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::string::String;
use std::thread_local;
use std::vec::Vec;

use oxidezap_plugin_abi as abi;

/// One command a plugin asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Send {
        chat: String,
        text: String,
    },
    Reply {
        chat: String,
        text: String,
        quoted: String,
    },
    MarkRead {
        chat: String,
        message: Option<String>,
    },
    Typing {
        chat: String,
        composing: bool,
    },
    Ui(Vec<u8>),
    Timer {
        delay_ms: i64,
        token: i64,
    },
}

/// What one event carries, by field.
#[derive(Debug, Default, Clone)]
pub struct Event {
    kind: i32,
    strings: BTreeMap<i32, String>,
    ints: BTreeMap<i32, i64>,
    lists: BTreeMap<i32, Vec<String>>,
}

impl Event {
    /// An event of `kind` with nothing in it.
    #[must_use]
    pub fn of(kind: i32) -> Self {
        Self {
            kind,
            ..Self::default()
        }
    }

    /// An ordinary incoming message.
    #[must_use]
    pub fn message(chat: &str, text: &str) -> Self {
        Self::of(abi::kinds::MESSAGE)
            .str(abi::fields::CHAT_JID, chat)
            .str(abi::fields::MESSAGE_ID, "3EB0")
            .str(abi::fields::TEXT, text)
    }

    /// Somebody using one of this plugin's widgets.
    #[must_use]
    pub fn action(id: &str, value: &str) -> Self {
        Self::of(abi::kinds::UI_ACTION)
            .str(abi::fields::ACTION_ID, id)
            .str(abi::fields::ACTION_VALUE, value)
    }

    /// A timer this plugin armed coming due.
    #[must_use]
    pub fn timer(token: i64) -> Self {
        Self::of(abi::kinds::TIMER).int(abi::fields::TIMER_TOKEN, token)
    }

    #[must_use]
    pub fn str(mut self, field: i32, value: &str) -> Self {
        self.strings.insert(field, value.to_owned());
        self
    }

    #[must_use]
    pub fn int(mut self, field: i32, value: i64) -> Self {
        self.ints.insert(field, value);
        self
    }

    #[must_use]
    pub fn flag(self, field: i32, value: bool) -> Self {
        self.int(field, i64::from(value))
    }

    #[must_use]
    pub fn list(mut self, field: i32, values: &[&str]) -> Self {
        self.lists
            .insert(field, values.iter().map(|s| (*s).to_owned()).collect());
        self
    }
}

#[derive(Default)]
struct State {
    event: Option<Event>,
    /// Handles for list elements: index into this, offset so `0` stays the
    /// event itself the way the ABI has it.
    children: Vec<String>,
    store: BTreeMap<String, String>,
    commands: Vec<Command>,
    name: Option<String>,
    subscription: i64,
    capabilities: i64,
    now_ms: i64,
}

thread_local! {
    static HOST: RefCell<State> = RefCell::new(State::default());
}

/// A plugin's world, for the length of a test.
///
/// Thread-local, because the imports it answers are free functions with
/// nowhere to carry a handle: two tests on two threads each get their own.
pub struct Host {
    _private: (),
}

impl Default for Host {
    fn default() -> Self {
        Self::new()
    }
}

impl Host {
    /// A fresh world: nothing stored, nothing said.
    #[must_use]
    pub fn new() -> Self {
        HOST.with(|h| *h.borrow_mut() = State::default());
        Self { _private: () }
    }

    /// Run the plugin's `oxi_init`, as the daemon does before anything else.
    pub fn init<D: crate::Declared>(&mut self, init: fn(crate::Setup) -> D) {
        let _ = init(crate::Setup::new());
    }

    /// Hand the plugin one event.
    pub fn deliver(&mut self, event: Event, handler: fn(&crate::Event)) {
        let kind = event.kind;
        HOST.with(|h| {
            let mut state = h.borrow_mut();
            state.children.clear();
            state.event = Some(event);
        });
        handler(&crate::Event::new(kind, 0));
    }

    /// Put a value in the plugin's store, as a previous run would have.
    pub fn store(&mut self, key: &str, value: &str) {
        HOST.with(|h| {
            h.borrow_mut()
                .store
                .insert(key.to_owned(), value.to_owned())
        });
    }

    /// What the plugin has kept.
    #[must_use]
    pub fn stored(&self, key: &str) -> Option<String> {
        HOST.with(|h| h.borrow().store.get(key).cloned())
    }

    /// Everything the plugin has asked for, in order.
    #[must_use]
    pub fn commands(&self) -> Vec<Command> {
        HOST.with(|h| h.borrow().commands.clone())
    }

    /// Just the messages, as `(chat, text)` — the common assertion.
    #[must_use]
    pub fn sent(&self) -> Vec<(String, String)> {
        self.commands()
            .into_iter()
            .filter_map(|c| match c {
                Command::Send { chat, text } | Command::Reply { chat, text, .. } => {
                    Some((chat, text))
                }
                _ => None,
            })
            .collect()
    }

    /// The widget tree the plugin last published, decoded.
    ///
    /// Through the same parser the daemon uses, so a tree this accepts is one
    /// that would have been drawn — and a malformed one fails here rather
    /// than in a log somewhere.
    ///
    /// # Errors
    ///
    /// The tree the plugin published, if it published one that does not
    /// decode.
    pub fn ui(&self) -> Option<Result<Vec<abi::ui::Node>, abi::ui::ParseError>> {
        self.commands().into_iter().rev().find_map(|c| match c {
            Command::Ui(bytes) => Some(abi::ui::parse(&bytes)),
            _ => None,
        })
    }

    /// What it called itself.
    #[must_use]
    pub fn name(&self) -> Option<String> {
        HOST.with(|h| h.borrow().name.clone())
    }

    /// The mask it subscribed with.
    #[must_use]
    pub fn subscription(&self) -> i64 {
        HOST.with(|h| h.borrow().subscription)
    }

    /// The mask it asked to be allowed.
    #[must_use]
    pub fn capabilities(&self) -> i64 {
        HOST.with(|h| h.borrow().capabilities)
    }

    /// What `oxi_now_ms` answers. Zero unless a test says otherwise, so a
    /// plugin that stamps something is testable at all.
    pub fn set_now_ms(&mut self, now: i64) {
        HOST.with(|h| h.borrow_mut().now_ms = now);
    }
}

// ---- what the stubs call -------------------------------------------------

pub(crate) fn set_name(name: &str) -> i32 {
    HOST.with(|h| h.borrow_mut().name = Some(name.to_owned()));
    abi::outcome::ACCEPTED
}

pub(crate) fn subscribe(mask: i64) {
    HOST.with(|h| h.borrow_mut().subscription = mask);
}

pub(crate) fn request_caps(mask: i64) {
    HOST.with(|h| h.borrow_mut().capabilities = mask);
}

pub(crate) fn now_ms() -> i64 {
    HOST.with(|h| h.borrow().now_ms)
}

pub(crate) fn command(command: Command) -> i32 {
    HOST.with(|h| h.borrow_mut().commands.push(command));
    abi::outcome::ACCEPTED
}

/// A string field, or `None` when the event does not carry it.
pub(crate) fn field_str(handle: i32, field: i32) -> Option<String> {
    HOST.with(|h| {
        let state = h.borrow();
        if handle != 0 {
            // A list element, whose only field is `SELF`.
            let at = usize::try_from(handle - 1).ok()?;
            return (field == abi::fields::SELF)
                .then(|| state.children.get(at).cloned())
                .flatten();
        }
        state.event.as_ref()?.strings.get(&field).cloned()
    })
}

pub(crate) fn field_i64(field: i32) -> i64 {
    HOST.with(|h| {
        h.borrow()
            .event
            .as_ref()
            .and_then(|e| e.ints.get(&field).copied())
            .unwrap_or(0)
    })
}

pub(crate) fn field_len(field: i32) -> i32 {
    HOST.with(|h| {
        h.borrow()
            .event
            .as_ref()
            .and_then(|e| e.lists.get(&field))
            .map_or(abi::ABSENT, |l| i32::try_from(l.len()).unwrap_or(0))
    })
}

pub(crate) fn field_at(field: i32, index: i32) -> i32 {
    HOST.with(|h| {
        let mut state = h.borrow_mut();
        let Some(value) = state
            .event
            .as_ref()
            .and_then(|e| e.lists.get(&field))
            .and_then(|l| usize::try_from(index).ok().and_then(|i| l.get(i)))
            .cloned()
        else {
            return abi::ABSENT;
        };
        state.children.push(value);
        i32::try_from(state.children.len()).unwrap_or(abi::ABSENT)
    })
}

pub(crate) fn kv_get(key: &str) -> Option<String> {
    HOST.with(|h| h.borrow().store.get(key).cloned())
}

pub(crate) fn kv_set(key: &str, value: &str) -> i32 {
    HOST.with(|h| {
        let mut state = h.borrow_mut();
        if value.is_empty() {
            state.store.remove(key);
        } else {
            state.store.insert(key.to_owned(), value.to_owned());
        }
    });
    abi::outcome::ACCEPTED
}
