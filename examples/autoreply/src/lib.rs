//! Answers messages containing a keyword, and lets you configure that from
//! the Settings screen.
//!
//! The whole plugin, and there is no allocator in it: every string it reads
//! lands in a fixed buffer on the stack, and the interface it publishes is
//! written into an array it owns. That is not a stunt — it is what makes the
//! release build a few kilobytes instead of hundreds, and it is the shape the
//! ABI is arranged to make natural.

#![no_std]

// The tests below run on the host, against the SDK's test host, and both use
// a heap. The plugin itself does not — that is the point of `no_std` here.
#[cfg(test)]
extern crate std;

use oxidezap_plugin::ui::{self, slot};
use oxidezap_plugin::{Caps, Declared, Event, Kinds, Setup, fields, kv, plugin, send_reply};

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

/// How much room every read of a setting makes.
///
/// One number for the reads and the write both, because two would mean a
/// keyword stored whole and matched on its first 64 bytes — answering text
/// nobody configured, and redrawing the truncated half into the field as
/// though that were the setting.
const SETTING: usize = 256;

const DEFAULT_KEYWORD: &str = "ping";
const DEFAULT_REPLY: &str = "pong";

/// What this plugin is, said once.
///
/// Each of these may be said only once, and the type says so: `name` is not a
/// method on what `name` returns. A second one used to be a refusal the
/// loader made, which is a plugin that does not run and an author reading a
/// log to find out why.
fn setup(p: Setup) -> impl Declared {
    let declared = p
        .name("Auto-reply")
        // Messages, and nothing else. An account's whole traffic is receipts
        // and presence; asking for kinds this never looks at would have the
        // daemon convert and queue every one of them for nothing.
        .subscribe(Kinds::MESSAGE)
        // Three, and the user sees all three before enabling this: "send
        // messages", "add buttons and settings", "keep its own settings".
        .capabilities(Caps::SEND | Caps::UI | Caps::STORAGE);
    // *After* the declaration, because publishing a tree needs `UI` and a
    // capability is not held until it has been asked for. Drawing first is
    // refused, silently as far as this function can see — which is why the
    // value the declaration returns is bound rather than returned directly.
    draw();
    declared
}

fn handle(ev: &Event) {
    match ev.kind() {
        oxidezap_plugin::abi::kinds::MESSAGE => handle_message(ev),
        oxidezap_plugin::abi::kinds::UI_ACTION => handle_action(ev),
        _ => {}
    }
}

fn handle_message(ev: &Event) {
    if !kv::flag(ON) {
        return;
    }
    // Our own messages, and messages the author has taken back. Answering
    // either is the classic way an autoreply embarrasses somebody: the first
    // makes it talk to itself, and the second answers a message that is no
    // longer there.
    if ev.flag(fields::FROM_ME) || ev.flag(fields::REVOKED) {
        return;
    }
    // Groups are left alone. A keyword that fires in a conversation of forty
    // people is a keyword that fires forty times.
    if ev.flag(fields::IS_GROUP) {
        return;
    }

    let text = ev.text(fields::TEXT);
    let keyword = kv::text::<SETTING>(KEYWORD, DEFAULT_KEYWORD);
    if !contains_ignoring_case(text.as_str(), keyword.as_str()) {
        return;
    }

    // `whole` rather than `as_str`: a JID that did not fit is not a shorter
    // JID, it is somebody else. This is the one read where truncation would
    // be actively wrong rather than merely incomplete, and the type is what
    // asks the question.
    let chat = ev.text(fields::CHAT_JID);
    let Some(chat) = chat.whole() else {
        return;
    };
    let message_id = ev.text(fields::MESSAGE_ID);
    let reply = kv::text::<SETTING>(REPLY, DEFAULT_REPLY);

    // As a reply rather than a fresh message: an automatic answer that does
    // not say what it is answering is indistinguishable from a person
    // suddenly speaking.
    send_reply(chat, reply.as_str(), message_id.as_str());
}

fn handle_action(ev: &Event) {
    let id = ev.text(fields::ACTION_ID);
    let value = ev.text(fields::ACTION_VALUE.sized::<SETTING>());

    // A value that did not fit is *not* a shorter value: storing it would
    // silently drop the end of somebody's keyword and then match on a word
    // they never typed.
    let (Some(id), Some(value)) = (id.whole(), value.whole()) else {
        oxidezap_plugin::log(
            oxidezap_plugin::level::WARN,
            "ignoring a setting longer than this plugin makes room for",
        );
        return;
    };

    match id {
        // A toggle's value is the state it is now in, not the one it was in:
        // the front end has already flipped it.
        ID_ON => {
            kv::set_flag(ON, value == "1");
        }
        ID_KEYWORD => store(KEYWORD, value),
        ID_REPLY => store(REPLY, value),
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
///
/// The section takes a closure, so there is no `end` to forget and the
/// widgets inside it have no slot to pass — a child carrying one is a tree
/// the daemon refuses.
fn draw() {
    let enabled = kv::flag(ON);
    let keyword = kv::text::<SETTING>(KEYWORD, DEFAULT_KEYWORD);
    let reply = kv::text::<SETTING>(REPLY, DEFAULT_REPLY);

    ui::publish::<1024>(|c| {
        c.section(slot::SETTINGS, "Auto-reply", |s| {
            s.toggle(ID_ON, "Reply automatically", enabled, true);
            // Drawn inert while the plugin is off, rather than hidden: a
            // setting that disappears reads as a setting that was lost.
            s.field(
                ID_KEYWORD,
                "When a message contains",
                keyword.as_str(),
                enabled,
            );
            s.field(ID_REPLY, "Reply with", reply.as_str(), enabled);
            s.label("One-to-one conversations only.");
        });
    });
}

fn store(key: &str, value: &str) {
    oxidezap_plugin::set(key, value);
}

/// Whether `haystack` contains `needle`, ignoring case for ASCII.
///
/// Ignoring case only where the answer is unambiguous: folding beyond ASCII
/// is a table this plugin will not carry, and a keyword that matched
/// differently depending on the language it was typed in would be worse than
/// one that is plainly case-sensitive there.
fn contains_ignoring_case(haystack: &str, needle: &str) -> bool {
    let (haystack, needle) = (haystack.as_bytes(), needle.as_bytes());
    if needle.is_empty() {
        return false;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use oxidezap_plugin::testing::{Event as In, Host};
    use std::string::ToString as _;
    use std::{format, vec};

    /// The whole point of the plugin, checked without a daemon: a message
    /// carrying the keyword is answered, in the chat it arrived in.
    #[test]
    fn it_answers_the_keyword_it_was_configured_with() {
        let mut host = Host::new();
        host.init(setup);
        host.store("enabled", "1");
        host.store("keyword", "oi");

        host.deliver(
            In::message("5511999@s.whatsapp.net", "oi, tudo bem?"),
            handle,
        );
        assert_eq!(
            host.sent(),
            [("5511999@s.whatsapp.net".to_string(), "pong".to_string())]
        );
    }

    /// Off until somebody turns it on. A plugin that starts answering the
    /// moment it is dropped in a folder is one that answered before anybody
    /// decided what it should say.
    #[test]
    fn it_says_nothing_until_it_is_switched_on() {
        let mut host = Host::new();
        host.init(setup);

        host.deliver(In::message("5511999@s.whatsapp.net", "ping"), handle);
        assert!(host.sent().is_empty());
    }

    /// The three ways an autoreply embarrasses somebody.
    #[test]
    fn it_leaves_groups_its_own_messages_and_revoked_ones_alone() {
        let mut host = Host::new();
        host.store("enabled", "1");

        let cases = [
            oxidezap_plugin::abi::fields::IS_GROUP,
            oxidezap_plugin::abi::fields::FROM_ME,
            oxidezap_plugin::abi::fields::REVOKED,
        ];
        for field in cases {
            host.deliver(
                In::message("5511999@s.whatsapp.net", "ping").flag(field, true),
                handle,
            );
            assert!(host.sent().is_empty(), "answered a message it should not");
        }
    }

    /// A toggle's action is the state it is now in, and the tree it
    /// republishes says so.
    #[test]
    fn pressing_its_own_toggle_stores_the_new_state_and_redraws() {
        let mut host = Host::new();
        host.deliver(In::action("enabled", "1"), handle);

        assert_eq!(host.stored("enabled").as_deref(), Some("1"));
        let tree = host.ui().expect("it published one").expect("it parses");
        let section = &tree[0];
        use oxidezap_plugin::abi::ui::flags;
        assert!(
            section.children[0].flags & flags::CHECKED != 0,
            "the switch is drawn on"
        );
        assert!(
            section.children[1].flags & flags::ENABLED != 0,
            "and the settings under it are editable again"
        );
    }

    /// A setting longer than this plugin makes room for is refused rather
    /// than stored short: half a keyword matches words nobody typed.
    #[test]
    fn a_setting_that_does_not_fit_is_not_stored() {
        let mut host = Host::new();
        let long = "x".repeat(SETTING + 1);
        host.deliver(In::action("keyword", &long), handle);

        assert_eq!(host.stored("keyword"), None);
    }

    /// The declaration a user is shown before enabling this.
    #[test]
    fn it_asks_for_three_things_and_only_messages() {
        let mut host = Host::new();
        host.init(setup);

        assert_eq!(host.name().as_deref(), Some("Auto-reply"));
        assert_eq!(
            host.capabilities(),
            (Caps::SEND | Caps::UI | Caps::STORAGE).bits()
        );
        assert_eq!(host.subscription(), Kinds::MESSAGE.bits());
    }
}
