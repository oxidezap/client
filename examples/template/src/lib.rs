//! A plugin that does almost nothing, so that what it does do is the shape of
//! the thing rather than the thing itself.
//!
//! It watches messages, keeps a count, and draws that count on the Settings
//! screen with a button that resets it. Nothing here touches the account, so
//! it runs the moment it is dropped in the plugins folder — asking to *send*
//! is what puts a question in front of the user, and the comment on
//! `capabilities` below says where.
//!
//! Copy this directory, change the name in `Cargo.toml`, and delete what you
//! do not need.

#![no_std]

// The tests at the bottom run on the host, against the SDK's test host, and
// both use a heap. The plugin itself does not.
#[cfg(test)]
extern crate std;

use oxidezap_plugin::ui::{self, slot};
use oxidezap_plugin::{Caps, Declared, Event, Kinds, Setup, Which, kv, log, plugin};

// Generates the three exports the host looks for, and the panic handler a
// `no_std` wasm module needs. `panic = own` opts out of the handler.
plugin!(init = setup, event = handle);

/// The key this plugin's own count is stored under.
const SEEN: &str = "seen";
/// The id of the button that resets it. The same string identifies the widget
/// in the tree and names the action that comes back.
const ID_RESET: &str = "reset";

/// What this plugin is, said once.
///
/// Each of these may be said only once, and the type enforces it: `name` is
/// not a method on what `name` returns.
fn setup(p: Setup) -> impl Declared {
    let declared = p
        .name("Template")
        // Ask for the kinds you actually read. An account's traffic is mostly
        // receipts and presence, and a plugin subscribed to those pays for
        // every one of them.
        .subscribe(Kinds::MESSAGE)
        // Drawing and keeping settings are things a plugin does only to
        // itself, so they take effect immediately. Adding `Caps::SEND` here —
        // or MARK_READ, or TYPING — is what makes the daemon ask the user
        // first, and nothing that acts on the account works until they agree.
        .capabilities(Caps::UI | Caps::STORAGE);
    // *After* the declaration: publishing a tree needs `UI` to have been
    // asked for, so drawing first is refused.
    draw();
    declared
}

/// One event, narrowed to what it is.
///
/// `which` hands back a view that names only the fields this kind carries, so
/// asking a message for a widget id is a method that is not there rather than
/// a field that reads back empty.
fn handle(ev: &Event) {
    match ev.which() {
        Which::Message(m) => {
            // Your own messages arrive too — this account writing from
            // another device — and so do ones their author has taken back.
            if m.from_me() || m.revoked() {
                return;
            }
            let count = count() + 1;
            log!(
                oxidezap_plugin::level::DEBUG,
                "message {count} in {}",
                m.chat().as_str()
            );
            set_count(count);
            draw();
        }
        Which::Action(a) => {
            if a.id().as_str() == ID_RESET {
                set_count(0);
                draw();
            }
        }
        _ => {}
    }
}

/// Publish the whole interface.
///
/// Whole every time, never a delta: the daemon compares what arrives against
/// what it holds and publishes nothing when they match, so redrawing on every
/// change costs a comparison rather than a frame.
fn draw() {
    let seen = kv::text::<24>(SEEN, "0");
    ui::publish::<512>(|c| {
        c.section(slot::SETTINGS, "Template", |s| {
            s.label(seen.as_str());
            s.button(ID_RESET, "Reset");
        });
    });
}

fn count() -> i64 {
    kv::text::<24>(SEEN, "0").as_str().parse().unwrap_or(0)
}

fn set_count(count: i64) {
    let mut line = oxidezap_plugin::Line::<24>::new();
    let _ = core::fmt::Write::write_fmt(&mut line, format_args!("{count}"));
    oxidezap_plugin::set(SEEN, line.as_str());
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidezap_plugin::testing::{Event as In, Host};

    #[test]
    fn it_counts_the_messages_it_is_given() {
        let mut host = Host::new();
        host.init(setup);
        host.deliver(In::message("5511999@s.whatsapp.net", "hi"), handle);
        host.deliver(In::message("5511999@s.whatsapp.net", "hi again"), handle);

        assert_eq!(host.stored("seen").as_deref(), Some("2"));
    }

    #[test]
    fn its_own_button_resets_the_count() {
        let mut host = Host::new();
        host.store("seen", "9");
        host.deliver(In::action(ID_RESET, ""), handle);

        assert_eq!(host.stored("seen").as_deref(), Some("0"));
    }

    /// What a user is shown before enabling it: a name, and a list that has
    /// nothing about the account in it.
    #[test]
    fn it_asks_for_nothing_that_touches_the_account() {
        let mut host = Host::new();
        host.init(setup);

        assert_eq!(host.name().as_deref(), Some("Template"));
        assert_eq!(
            host.capabilities() & oxidezap_plugin::abi::caps::NEEDS_APPROVAL,
            0
        );
    }
}
