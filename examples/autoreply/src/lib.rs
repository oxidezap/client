//! Answers messages containing a keyword, and lets you configure that from
//! the Settings screen.
//!
//! The whole plugin, and there is no allocator in it: every string it reads
//! lands in a fixed buffer on the stack, and the interface it publishes is
//! written into an array it owns. That is not a stunt — it is what makes the
//! release build tens of kilobytes instead of hundreds, and it is the shape
//! the ABI is arranged to make natural.

#![no_std]

use oxidezap_plugin::{Event, Setup, Text, abi, plugin, send_reply, set_ui};

plugin!(init = setup, event = handle);

/// Keys in this plugin's own store.
const ON: &str = "enabled";
const KEYWORD: &str = "keyword";
const REPLY: &str = "reply";

/// Widget ids. The same strings identify a widget in the tree and name the
/// action that comes back, which is what makes [`handle_action`] a match on
/// one value rather than a lookup.
const ID_ON: &str = "enabled";
const ID_KEYWORD: &str = "keyword";
const ID_REPLY: &str = "reply";

const DEFAULT_KEYWORD: &str = "ping";
const DEFAULT_REPLY: &str = "pong";

fn setup(p: &mut Setup) {
    p.name("Resposta automática");
    // Messages, and nothing else. An account's whole traffic is receipts and
    // presence; asking for kinds this never looks at would have the daemon
    // convert and queue every one of them for nothing.
    p.subscribe(abi::kinds::bit(abi::kinds::MESSAGE));
    // Three, and the user sees all three before enabling this: "send
    // messages", "add buttons and settings", "keep its own settings".
    p.capabilities(abi::caps::SEND | abi::caps::UI | abi::caps::STORAGE);
    draw();
}

fn handle(ev: &Event) {
    match ev.kind() {
        abi::kinds::MESSAGE => handle_message(ev),
        abi::kinds::UI_ACTION => handle_action(ev),
        _ => {}
    }
}

fn handle_message(ev: &Event) {
    if !enabled() {
        return;
    }
    // Our own messages, and messages the author has taken back. Answering
    // either is the classic way an autoreply embarrasses somebody: the first
    // makes it talk to itself, and the second answers a message that is no
    // longer there.
    if ev.flag(abi::fields::FROM_ME) || ev.flag(abi::fields::REVOKED) {
        return;
    }
    // Groups are left alone. A keyword that fires in a conversation of forty
    // people is a keyword that fires forty times.
    if ev.flag(abi::fields::IS_GROUP) {
        return;
    }

    let text = ev.text::<512>(abi::fields::TEXT);
    let keyword = setting::<64>(KEYWORD, DEFAULT_KEYWORD);
    if !contains_ignoring_case(text.as_str(), keyword.as_str()) {
        return;
    }

    let chat = ev.text::<128>(abi::fields::CHAT_JID);
    // A JID that did not fit is not a shorter JID, it is somebody else. This
    // is the one place where a truncated read would be actively wrong rather
    // than merely incomplete.
    if !chat.complete() {
        return;
    }
    let message_id = ev.text::<128>(abi::fields::MESSAGE_ID);
    let reply = setting::<256>(REPLY, DEFAULT_REPLY);

    // As a reply rather than a fresh message: an automatic answer that does
    // not say what it is answering is indistinguishable from a person
    // suddenly speaking.
    send_reply(chat.as_str(), reply.as_str(), message_id.as_str());
}

fn handle_action(ev: &Event) {
    let id = ev.text::<32>(abi::fields::ACTION_ID);
    let value = ev.text::<256>(abi::fields::ACTION_VALUE);

    // A value that did not fit is *not* a shorter value: storing it would
    // silently drop the end of somebody's keyword and then match on a word
    // they never typed. `Text::complete` is the whole reason the read reports
    // the full length rather than only what it wrote.
    if !id.complete() || !value.complete() {
        oxidezap_plugin::log(
            oxidezap_plugin::level::WARN,
            "ignoring a setting longer than this plugin makes room for",
        );
        return;
    }

    match id.as_str() {
        // A toggle's value is the state it is now in, not the one it was in:
        // the front end has already flipped it.
        ID_ON => store(ON, if value.as_str() == "1" { "1" } else { "0" }),
        ID_KEYWORD => store(KEYWORD, value.as_str()),
        ID_REPLY => store(REPLY, value.as_str()),
        _ => return,
    }
    // Redraw, because the tree carries the values: the toggle it published a
    // moment ago still says what it used to.
    draw();
}

/// Publish the whole interface.
///
/// Whole every time, never a delta. The daemon compares what arrives against
/// what it holds and publishes nothing when they match, so redrawing on every
/// change costs a comparison rather than a frame.
fn draw() {
    let enabled = enabled();
    let keyword = setting::<64>(KEYWORD, DEFAULT_KEYWORD);
    let reply = setting::<256>(REPLY, DEFAULT_REPLY);

    let mut buf = [0u8; 1024];
    let mut w = abi::ui::Writer::new(&mut buf);

    w.begin(
        abi::ui::kind::SECTION,
        abi::ui::slot::SETTINGS,
        abi::ui::flags::ENABLED,
        "",
        "Resposta automática",
        "",
    );
    w.leaf(
        abi::ui::kind::TOGGLE,
        abi::ui::slot::NONE,
        abi::ui::flags::ENABLED | if enabled { abi::ui::flags::CHECKED } else { 0 },
        ID_ON,
        "Responder sozinho",
        if enabled { "1" } else { "0" },
    );
    // Drawn inert while the plugin is off, rather than hidden: a setting that
    // disappears reads as a setting that was lost.
    let editable = if enabled { abi::ui::flags::ENABLED } else { 0 };
    w.leaf(
        abi::ui::kind::TEXT_FIELD,
        abi::ui::slot::NONE,
        editable,
        ID_KEYWORD,
        "Quando a mensagem contiver",
        keyword.as_str(),
    );
    w.leaf(
        abi::ui::kind::TEXT_FIELD,
        abi::ui::slot::NONE,
        editable,
        ID_REPLY,
        "Responder com",
        reply.as_str(),
    );
    w.leaf(
        abi::ui::kind::LABEL,
        abi::ui::slot::NONE,
        0,
        "",
        "Só em conversas de duas pessoas.",
        "",
    );
    w.end();

    if let Ok(len) = w.finish() {
        set_ui(&buf[..len]);
    }
}

fn enabled() -> bool {
    // Off until somebody turns it on. A plugin that starts answering the
    // moment it is dropped in a folder is one that answered before anybody
    // decided what it should say.
    oxidezap_plugin::get::<8>(ON).as_str() == "1"
}

/// A stored value, or the default when nothing is stored.
fn setting<const N: usize>(key: &str, fallback: &'static str) -> Setting<N> {
    let stored = oxidezap_plugin::get::<N>(key);
    if stored.is_empty() {
        Setting::Default(fallback)
    } else {
        Setting::Stored(stored)
    }
}

fn store(key: &str, value: &str) {
    oxidezap_plugin::set(key, value);
}

/// Either what is stored or what this plugin ships with.
///
/// An enum rather than a `String`, because there is no allocator: the stored
/// half owns a stack buffer and the default half is a `'static` the binary
/// already carries.
enum Setting<const N: usize> {
    Stored(Text<N>),
    Default(&'static str),
}

impl<const N: usize> Setting<N> {
    fn as_str(&self) -> &str {
        match self {
            Self::Stored(text) => text.as_str(),
            Self::Default(value) => value,
        }
    }
}

/// Case-insensitive substring search over ASCII.
///
/// Hand-written because `str::to_lowercase` allocates and because full
/// Unicode case folding is a table this plugin has no business carrying. What
/// it costs is that a keyword in a script with case beyond ASCII matches only
/// exactly, which for a keyword somebody chose themselves is a fair trade.
fn contains_ignoring_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let (haystack, needle) = (haystack.as_bytes(), needle.as_bytes());
    if needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

/// Nothing here unwinds — the profile aborts — but `wasm32-unknown-unknown`
/// with `no_std` still needs the handler to exist.
#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // A trap, which the host turns into "this plugin stopped, and why". The
    // alternative — a silent loop — would be a plugin that burns its fuel
    // budget on every event forever.
    core::arch::wasm32::unreachable()
}
