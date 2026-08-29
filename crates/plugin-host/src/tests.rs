//! The host's behaviour, against modules written for these tests.
//!
//! Fixtures are hand-written `.wat` assembled at test time rather than
//! checked-in `.wasm`. A binary in the tree is a thing nobody reviews, and
//! building one in CI would put a wasm toolchain there for the sake of these
//! tests — while a twelve-line `.wat` says what misbehaviour it reproduces
//! far better than the Rust that would compile to it.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// The library's clock rather than std's, like everywhere else in this tree:
// a test that moves time has to move what these read.
use wacore::time::Instant;

use oxidezap_core::{ChatMessage, MessageStatus, PluginSlot, PluginWidget};

use super::*;

// ---- doubles -------------------------------------------------------------

/// A [`Commands`] that records what it was asked for.
struct Recorder {
    sent: Mutex<Vec<(String, String, Option<String>)>>,
    answer: Outcome,
    /// Held closed to park the plugin's thread inside a command, which is how
    /// a test builds up a backlog it can then act on.
    gate: AtomicBool,
}

impl Recorder {
    fn new(answer: Outcome) -> Arc<Self> {
        Arc::new(Self {
            sent: Mutex::new(Vec::new()),
            answer,
            gate: AtomicBool::new(true),
        })
    }

    fn sent(&self) -> Vec<(String, String, Option<String>)> {
        self.sent.lock().expect("not poisoned").clone()
    }

    fn close_gate(&self) {
        self.gate.store(false, Ordering::SeqCst);
    }

    fn open_gate(&self) {
        self.gate.store(true, Ordering::SeqCst);
    }
}

impl Commands for Arc<Recorder> {
    fn send_text(&self, jid: &str, text: &str, quoted: Option<&str>) -> Outcome {
        while !self.gate.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.sent.lock().expect("not poisoned").push((
            jid.to_owned(),
            text.to_owned(),
            quoted.map(str::to_owned),
        ));
        self.answer
    }
    fn mark_read(&self, _jid: &str, _message_id: Option<&str>) -> Outcome {
        Outcome::Accepted
    }
    fn typing(&self, _jid: &str, _composing: bool) -> Outcome {
        Outcome::Accepted
    }
}

/// Every set of surfaces the host published, newest last.
#[derive(Clone, Default)]
struct Published(Arc<Mutex<Vec<Vec<PluginSurface>>>>);

impl Published {
    fn sink(&self) -> Sink {
        let inner = Arc::clone(&self.0);
        Arc::new(move |surfaces| inner.lock().expect("not poisoned").push(surfaces))
    }

    fn latest(&self) -> Vec<PluginSurface> {
        self.0
            .lock()
            .expect("not poisoned")
            .last()
            .cloned()
            .unwrap_or_default()
    }

    /// Wait for `predicate` to hold of the newest publication.
    ///
    /// Plugins run on their own threads, so every assertion about what one
    /// did is an assertion about something that has not necessarily happened
    /// yet. Polling with a deadline is what keeps these tests from being
    /// either flaky or a fixed sleep.
    fn settles(
        &self,
        what: &str,
        predicate: impl Fn(&[PluginSurface]) -> bool,
    ) -> Vec<PluginSurface> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let latest = self.latest();
            if predicate(&latest) {
                return latest;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Wait for a condition on the recorder, with the same reasoning.
fn until(what: &str, predicate: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A directory that removes itself.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "oxidezap-plugins-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a writable temp dir");
        Self(path)
    }

    /// Assemble a `.wat` fixture into the directory under `name`.
    fn plugin(&self, name: &str, wat: &str) -> &Self {
        let wasm = wat::parse_str(wat).expect("the fixture assembles");
        std::fs::write(self.0.join(format!("{name}.wasm")), wasm).expect("writable");
        self
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn message(chat: &str, text: &str) -> UiEvent {
    UiEvent::MessageReceived {
        chat_jid: chat.into(),
        message: Box::new(ChatMessage {
            id: "MSG1".into(),
            sender: chat.into(),
            sender_name: None,
            content: text.into(),
            timestamp: chrono::DateTime::from_timestamp_millis(1_700_000_000_000)
                .expect("a valid instant"),
            is_from_me: false,
            is_read: false,
            media: None,
            reactions: Default::default(),
            status: MessageStatus::Delivered,
            quoted: None,
            revoked: false,
            system: None,
        }),
        sender_name: None,
    }
}

// ---- fixtures ------------------------------------------------------------

/// Subscribes to messages, asks for `send`, and answers every message with
/// "pong" in the chat it arrived in.
fn pong() -> String {
    versioned(PONG)
}

const PONG: &str = r#"(module
  (import "oxidezap" "oxi_subscribe"     (func $subscribe (param i64)))
  (import "oxidezap" "oxi_request_caps"  (func $caps (param i64)))
  (import "oxidezap" "oxi_field_str"     (func $field_str (param i32 i32 i32 i32) (result i32)))
  (import "oxidezap" "oxi_send_text"     (func $send (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "pong")
  (func (export "oxi_abi_version") (result i32) (i32.const $ABI_VERSION))
  (global $answer (mut i32) (i32.const 0))
  (func (export "oxi_init") (result i32)
    (call $subscribe (i64.const 2))   ;; 1 << kinds::MESSAGE
    (call $caps      (i64.const 1))   ;; caps::SEND
    (i32.const 0))
  (func (export "oxi_on_event") (param $kind i32) (param $ev i32) (result i32)
    (local $n i32)
    ;; fields::CHAT_JID into a scratch buffer well past the "pong" literal
    (local.set $n (call $field_str (local.get $ev) (i32.const 1) (i32.const 256) (i32.const 256)))
    (if (i32.gt_s (local.get $n) (i32.const 0))
      (then
        (global.set $answer
          (call $send (i32.const 256) (local.get $n) (i32.const 0) (i32.const 4)))))
    (i32.const 0))
  (func (export "answer") (result i32) (global.get $answer))
)"#;

/// The same, but never asks for the capability.
fn pong_without_permission() -> String {
    versioned(PONG_WITHOUT_PERMISSION)
}

const PONG_WITHOUT_PERMISSION: &str = r#"(module
  (import "oxidezap" "oxi_subscribe" (func $subscribe (param i64)))
  (import "oxidezap" "oxi_field_str" (func $field_str (param i32 i32 i32 i32) (result i32)))
  (import "oxidezap" "oxi_send_text" (func $send (param i32 i32 i32 i32) (result i32)))
  (import "oxidezap" "oxi_ui_set"    (func $ui_set (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "pong")
  (func (export "oxi_abi_version") (result i32) (i32.const $ABI_VERSION))
  (func (export "oxi_init") (result i32)
    (call $subscribe (i64.const 2))
    (i32.const 0))
  (func (export "oxi_on_event") (param $kind i32) (param $ev i32) (result i32)
    (local $n i32)
    (local.set $n (call $field_str (local.get $ev) (i32.const 1) (i32.const 256) (i32.const 256)))
    (drop (call $send (i32.const 256) (local.get $n) (i32.const 0) (i32.const 4)))
    ;; And a tree it also never asked to be allowed to draw.
    (drop (call $ui_set (i32.const 0) (i32.const 4)))
    (i32.const 0))
)"#;

fn spins() -> String {
    versioned(SPINS)
}

const SPINS: &str = r#"(module
  (import "oxidezap" "oxi_subscribe" (func $subscribe (param i64)))
  (memory (export "memory") 1)
  (func (export "oxi_abi_version") (result i32) (i32.const $ABI_VERSION))
  (func (export "oxi_init") (result i32) (call $subscribe (i64.const 2)) (i32.const 0))
  (func (export "oxi_on_event") (param i32 i32) (result i32) (loop (br 0)) (i32.const 0))
)"#;

/// Asks for one page and then for far more than the limit allows.
fn greedy() -> String {
    versioned(GREEDY)
}

const GREEDY: &str = r#"(module
  (import "oxidezap" "oxi_subscribe" (func $subscribe (param i64)))
  (memory (export "memory") 1)
  (func (export "oxi_abi_version") (result i32) (i32.const $ABI_VERSION))
  (func (export "oxi_init") (result i32) (call $subscribe (i64.const 2)) (i32.const 0))
  (func (export "oxi_on_event") (param i32 i32) (result i32)
    (drop (memory.grow (i32.const 4096)))
    (i32.const 0))
)"#;

/// Deliberately not ASCII: a length in characters rather than bytes would
/// truncate this, which is the mistake the ABI's snprintf convention exists
/// to make visible.
const NAME: &str = "Saudações";

/// Asks for four far-future timers on every message, and reports back the
/// first refusal it is given.
const ARMS_TIMERS: &str = r#"(module
  (import "oxidezap" "oxi_subscribe"    (func $subscribe (param i64)))
  (import "oxidezap" "oxi_request_caps" (func $caps (param i64)))
  (import "oxidezap" "oxi_timer_set"    (func $timer (param i64 i64) (result i32)))
  (import "oxidezap" "oxi_send_text"    (func $send (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "a@s.whatsapp.net")
  (data (i32.const 64) "refused")
  (func (export "oxi_abi_version") (result i32) (i32.const $ABI_VERSION))
  (func (export "oxi_init") (result i32)
    (call $subscribe (i64.const 2))         ;; messages
    (call $caps (i64.const 33))             ;; caps::SEND | caps::TIMERS
    (i32.const 0))
  (func (export "oxi_on_event") (param $kind i32) (param $ev i32) (result i32)
    (local $i i32)
    (local $answer i32)
    (block $done
      (loop $next
        (br_if $done (i32.ge_s (local.get $i) (i32.const 4)))
        ;; An hour out, so none of these ever fires and they only accumulate.
        (local.set $answer (call $timer (i64.const 3600000) (i64.const 1)))
        (if (i32.lt_s (local.get $answer) (i32.const 0))
          (then
            (drop (call $send (i32.const 0) (i32.const 16) (i32.const 64) (i32.const 7)))
            (br $done)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $next)))
    (i32.const 0))
)"#;

fn arms_timers() -> String {
    versioned(ARMS_TIMERS)
}

/// Stamp a fixture with the ABI version this build speaks.
///
/// Every fixture goes through this rather than writing the number itself, so
/// that bumping `abi::VERSION` renames one failure — the version test — rather
/// than turning every other test into "nothing loaded", which reads like a
/// discovery or entry-point bug and sends the reader to the wrong file.
fn versioned(wat: &str) -> String {
    wat.replace("$ABI_VERSION", &abi::VERSION.to_string())
}

fn wat_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("\\{b:02x}")).collect()
}

/// A plugin whose whole interface is published from `oxi_init`.
fn draws() -> String {
    let mut buf = vec![0u8; 512];
    let mut w = abi::ui::Writer::new(&mut buf);
    w.leaf(
        abi::ui::kind::BUTTON,
        abi::ui::slot::CHAT_HEADER,
        abi::ui::flags::ENABLED,
        "greet",
        "Cumprimentar",
        "",
    );
    let n = w.finish().expect("fits");
    versioned(&format!(
        r#"(module
  (import "oxidezap" "oxi_request_caps" (func $caps (param i64)))
  (import "oxidezap" "oxi_set_name"     (func $set_name (param i32 i32) (result i32)))
  (import "oxidezap" "oxi_ui_set"       (func $ui_set (param i32 i32) (result i32)))
  (import "oxidezap" "oxi_field_str"    (func $field_str (param i32 i32 i32 i32) (result i32)))
  (import "oxidezap" "oxi_send_text"    (func $send (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "{tree}")
  (data (i32.const 1024) "{name}")
  (data (i32.const 1100) "oi")
  (func (export "oxi_abi_version") (result i32) (i32.const $ABI_VERSION))
  (func (export "oxi_init") (result i32)
    (call $caps (i64.const 9))    ;; caps::SEND | caps::UI
    (drop (call $set_name (i32.const 1024) (i32.const {name_len})))
    (drop (call $ui_set (i32.const 0) (i32.const {len})))
    (i32.const 0))
  (func (export "oxi_on_event") (param $kind i32) (param $ev i32) (result i32)
    (local $n i32)
    ;; A UI action carries the chat the window had open: answer into it.
    (local.set $n (call $field_str (local.get $ev) (i32.const 1) (i32.const 2048) (i32.const 256)))
    (if (i32.gt_s (local.get $n) (i32.const 0))
      (then (drop (call $send (i32.const 2048) (local.get $n) (i32.const 1100) (i32.const 2)))))
    (i32.const 0))
)"#,
        tree = wat_bytes(&buf[..n]),
        len = n,
        name = NAME,
        // Bytes, not characters: the ABI counts what crosses it, and getting
        // this wrong is exactly how a name loses its last letter.
        name_len = NAME.len()
    ))
}

// ---- what the host does --------------------------------------------------

fn host(dir: &TempDir, commands: Arc<Recorder>, published: &Published) -> Plugins {
    let plugins = Plugins::load(&dir.0, None, Arc::new(commands), published.sink());
    // Nothing acts on the account until somebody says so, so a test about
    // what a plugin *does* has to say so first. Written out here rather than
    // hidden in the constructor: the gate is the point, and a helper that
    // silently opened it would make every test below prove nothing.
    for id in plugins
        .ids()
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>()
    {
        plugins.approve(&id, true);
    }
    plugins
}

/// The same, with nobody having agreed to anything.
fn unapproved_host(dir: &TempDir, commands: Arc<Recorder>, published: &Published) -> Plugins {
    Plugins::load(&dir.0, None, Arc::new(commands), published.sink())
}

#[test]
fn a_plugin_sees_a_message_and_answers_it() {
    let dir = TempDir::new("pong");
    dir.plugin("autoreply", &pong());
    let commands = Recorder::new(Outcome::Accepted);
    let published = Published::default();
    let plugins = host(&dir, Arc::clone(&commands), &published);

    plugins.observe(&message("5511999@s.whatsapp.net", "ping"));
    until("the reply", || commands.sent().len() == 1);

    assert_eq!(
        commands.sent()[0],
        ("5511999@s.whatsapp.net".into(), "pong".into(), None)
    );
}

/// The declaration is the contract. A plugin that never asked to send is
/// refused at the import, not at the session.
#[test]
fn a_command_outside_the_declared_capabilities_never_reaches_the_daemon() {
    let dir = TempDir::new("denied");
    dir.plugin("sneaky", &pong_without_permission());
    let commands = Recorder::new(Outcome::Accepted);
    let published = Published::default();
    let plugins = host(&dir, Arc::clone(&commands), &published);

    plugins.observe(&message("a@s.whatsapp.net", "ping"));
    // Nothing to wait *for*, so wait for the plugin to have run at all: it
    // publishes no tree either, and both refusals are the same call.
    std::thread::sleep(Duration::from_millis(200));

    assert!(commands.sent().is_empty(), "it may not send");
    let surface = published.latest().remove(0);
    assert!(surface.roots.is_empty(), "and it may not draw");
    assert!(surface.capabilities.is_empty());
    assert!(surface.is_running(), "but it is not stopped for trying");
}

/// The number that makes running a stranger's code in this process
/// defensible.
#[test]
fn a_plugin_that_loops_forever_runs_out_of_fuel_and_is_stopped() {
    let dir = TempDir::new("spin");
    dir.plugin("spinner", &spins());
    let published = Published::default();
    let plugins = host(&dir, Recorder::new(Outcome::Accepted), &published);

    plugins.observe(&message("a@s.whatsapp.net", "go"));
    let surfaces = published.settles("the plugin to be stopped", |s| {
        s.first().is_some_and(|p| !p.is_running())
    });

    let reason = surfaces[0].stopped.clone().expect("a reason");
    assert!(
        reason.to_lowercase().contains("fuel"),
        "the reason should name what happened, got {reason:?}"
    );
}

#[test]
fn a_plugin_that_asks_for_more_memory_than_it_may_have_is_stopped() {
    let dir = TempDir::new("greedy");
    dir.plugin("greedy", &greedy());
    let published = Published::default();
    let plugins = host(&dir, Recorder::new(Outcome::Accepted), &published);

    plugins.observe(&message("a@s.whatsapp.net", "go"));
    published.settles("the plugin to be stopped", |s| {
        s.first().is_some_and(|p| !p.is_running())
    });
}

#[test]
fn a_module_built_for_another_abi_is_refused_before_it_runs() {
    let dir = TempDir::new("version");
    // Stamped with a version this host does not speak. Written by
    // substituting the placeholder directly rather than by patching what
    // `versioned` rendered, so the two cannot disagree about what a fixture
    // looks like.
    dir.plugin(
        "future",
        &SPINS.replace("$ABI_VERSION", &(abi::VERSION + 1).to_string()),
    );
    let published = Published::default();
    let plugins = host(&dir, Recorder::new(Outcome::Accepted), &published);

    assert!(plugins.is_empty());
    assert!(published.latest().is_empty());
}

#[test]
fn a_module_missing_the_entry_point_is_refused() {
    let dir = TempDir::new("no-entry");
    dir.plugin(
        "broken",
        &versioned(
            r#"(module
             (memory (export "memory") 1)
             (func (export "oxi_abi_version") (result i32) (i32.const $ABI_VERSION))
             (func (export "oxi_init") (result i32) (i32.const 0)))"#,
        ),
    );
    let published = Published::default();
    let plugins = host(&dir, Recorder::new(Outcome::Accepted), &published);
    assert!(plugins.is_empty());
}

/// A file that is not a plugin is one file, not a reason to serve no account.
#[test]
fn one_unloadable_file_does_not_stop_the_others() {
    let dir = TempDir::new("mixed");
    dir.plugin("autoreply", &pong());
    std::fs::write(dir.0.join("junk.wasm"), b"not wasm at all").expect("writable");
    std::fs::write(dir.0.join("notes.txt"), b"ignored").expect("writable");

    let commands = Recorder::new(Outcome::Accepted);
    let published = Published::default();
    let plugins = host(&dir, Arc::clone(&commands), &published);

    assert_eq!(published.latest().len(), 1);
    plugins.observe(&message("a@s.whatsapp.net", "ping"));
    until("the reply", || commands.sent().len() == 1);
}

/// The subscription mask is what keeps an account's whole traffic from being
/// converted and queued for a plugin that wants none of it.
#[test]
fn a_plugin_is_handed_only_the_kinds_it_asked_for() {
    let dir = TempDir::new("subscription");
    dir.plugin("autoreply", &pong());
    let commands = Recorder::new(Outcome::Accepted);
    let published = Published::default();
    let plugins = host(&dir, Arc::clone(&commands), &published);

    // It subscribed to messages only.
    plugins.observe(&UiEvent::Connected);
    plugins.observe(&UiEvent::Disconnected("bye".into()));
    std::thread::sleep(Duration::from_millis(150));
    assert!(commands.sent().is_empty());

    plugins.observe(&message("a@s.whatsapp.net", "ping"));
    until("the reply", || commands.sent().len() == 1);
}

#[test]
fn a_plugin_publishes_its_interface_before_any_event() {
    let dir = TempDir::new("draws");
    dir.plugin("greeter", &draws());
    let published = Published::default();
    let _plugins = host(&dir, Recorder::new(Outcome::Accepted), &published);

    let surfaces = published.settles("the interface", |s| {
        s.first().is_some_and(|p| !p.roots.is_empty())
    });
    let surface = &surfaces[0];
    assert_eq!(surface.id, "greeter");
    assert_eq!(surface.name, "Saudações");
    assert_eq!(surface.roots[0].slot, PluginSlot::ChatHeader);
    assert_eq!(surface.roots[0].node.widget, PluginWidget::Button);
    assert_eq!(surface.roots[0].node.label, "Cumprimentar");
    assert!(surface.roots[0].node.enabled);
    assert_eq!(
        surface.capabilities,
        vec![
            "send messages".to_string(),
            "add buttons and settings".into()
        ]
    );
}

/// The whole loop the design turns on: a button drawn from daemon state, a
/// click routed back into the sandbox, and a command out the other side.
#[test]
fn pressing_a_plugins_button_reaches_the_plugin_with_the_open_chat() {
    let dir = TempDir::new("action");
    dir.plugin("greeter", &draws());
    let commands = Recorder::new(Outcome::Accepted);
    let published = Published::default();
    let plugins = host(&dir, Arc::clone(&commands), &published);
    published.settles("the interface", |s| {
        s.first().is_some_and(|p| !p.roots.is_empty())
    });

    plugins.act(&PluginAction {
        plugin: "greeter".into(),
        action: "greet".into(),
        value: None,
        chat_jid: Some("5511999@s.whatsapp.net".into()),
        slot: PluginSlot::ChatHeader,
    });

    until("the greeting", || commands.sent().len() == 1);
    assert_eq!(
        commands.sent()[0],
        ("5511999@s.whatsapp.net".into(), "oi".into(), None)
    );
}

#[test]
fn an_action_for_a_plugin_that_is_not_loaded_is_ignored() {
    let dir = TempDir::new("stray");
    dir.plugin("greeter", &draws());
    let plugins = host(
        &dir,
        Recorder::new(Outcome::Accepted),
        &Published::default(),
    );
    plugins.act(&PluginAction {
        plugin: "nobody".into(),
        action: "x".into(),
        value: None,
        chat_jid: None,
        slot: PluginSlot::Settings,
    });
}

/// A window's frame can be older than the daemon's state, so an action has
/// to be checked against the tree the plugin last published rather than
/// against the plugin merely being loaded.
#[test]
fn an_action_for_a_widget_the_plugin_does_not_draw_is_ignored() {
    let dir = TempDir::new("stale-action");
    dir.plugin("greeter", &draws());
    let commands = Recorder::new(Outcome::Accepted);
    let published = Published::default();
    let plugins = host(&dir, Arc::clone(&commands), &published);
    published.settles("the interface", |s| {
        s.first().is_some_and(|p| !p.roots.is_empty())
    });

    // An id this plugin never published, which is a handler asked about a
    // widget that does not exist.
    plugins.act(&PluginAction {
        plugin: "greeter".into(),
        action: "farewell".into(),
        value: None,
        chat_jid: Some("5511999@s.whatsapp.net".into()),
        slot: PluginSlot::ChatHeader,
    });
    std::thread::sleep(Duration::from_millis(200));
    assert!(commands.sent().is_empty());

    // Nor in a slot it does not draw it in. One plugin may use the same id in
    // a header and in its settings panel — two widgets — so withdrawing one
    // must not leave the other vouching for it.
    plugins.act(&PluginAction {
        plugin: "greeter".into(),
        action: "greet".into(),
        value: None,
        chat_jid: None,
        slot: PluginSlot::Settings,
    });
    std::thread::sleep(Duration::from_millis(200));
    assert!(commands.sent().is_empty());

    // And the one it does draw, where it draws it, still works — so this is
    // about the id and the slot rather than about actions having stopped
    // arriving.
    plugins.act(&PluginAction {
        plugin: "greeter".into(),
        action: "greet".into(),
        value: None,
        chat_jid: Some("5511999@s.whatsapp.net".into()),
        slot: PluginSlot::ChatHeader,
    });
    until("the greeting", || commands.sent().len() == 1);
}

#[test]
fn plugins_load_in_a_stable_order() {
    let dir = TempDir::new("order");
    dir.plugin("zeta", &draws());
    dir.plugin("alpha", &draws());
    let published = Published::default();
    let _plugins = host(&dir, Recorder::new(Outcome::Accepted), &published);

    let ids: Vec<String> = published.latest().into_iter().map(|s| s.id).collect();
    assert_eq!(ids, vec!["alpha", "zeta"]);
}

#[test]
fn an_absent_directory_is_not_an_error() {
    let published = Published::default();
    let plugins = Plugins::load(
        Path::new("/nonexistent/oxidezap/plugins"),
        None,
        Arc::new(Recorder::new(Outcome::Accepted)),
        published.sink(),
    );
    assert!(plugins.is_empty());
}

/// A plugin id is also the stem of its settings file, so one carrying a
/// separator would name a path of its own choosing.
#[test]
fn a_file_whose_name_is_not_a_usable_id_is_skipped() {
    assert_eq!(
        plugin_id(Path::new("a/autoreply.wasm")).as_deref(),
        Some("autoreply")
    );
    assert_eq!(
        plugin_id(Path::new("a/auto-reply_2.wasm")).as_deref(),
        Some("auto-reply_2")
    );
    assert_eq!(
        plugin_id(Path::new("a/../escape.wasm")).as_deref(),
        Some("escape")
    );
    assert_eq!(plugin_id(Path::new("a/with space.wasm")), None);
    assert_eq!(plugin_id(Path::new("a/dots.in.name.wasm")), None);
    assert_eq!(plugin_id(Path::new("a/.wasm")), None);
}

/// What a command answered reaches the plugin, which is the thing a socket
/// front end cannot learn about its own.
#[test]
fn a_refused_command_is_reported_back_into_the_sandbox() {
    let dir = TempDir::new("refused");
    dir.plugin("autoreply", &pong());
    let commands = Recorder::new(Outcome::NoSession);
    let published = Published::default();
    let plugins = host(&dir, Arc::clone(&commands), &published);

    plugins.observe(&message("a@s.whatsapp.net", "ping"));
    until("the attempt", || commands.sent().len() == 1);
    // The plugin stashed the answer in a global; reading it back through the
    // instance is not something the host exposes, so what this asserts is
    // that a refusal is not a trap: it kept running.
    std::thread::sleep(Duration::from_millis(100));
    assert!(published.latest()[0].is_running());
}

#[test]
fn shutting_down_joins_every_plugin() {
    let dir = TempDir::new("shutdown");
    dir.plugin("autoreply", &pong());
    dir.plugin("greeter", &draws());
    let plugins = host(
        &dir,
        Recorder::new(Outcome::Accepted),
        &Published::default(),
    );
    assert_eq!(plugins.ids(), vec!["autoreply", "greeter"]);
    plugins.shutdown();
    // Idempotent: the daemon calls this on its way out and `Drop` calls it
    // again.
    plugins.shutdown();
}

/// The real SDK, against the real host.
///
/// Ignored by default because it needs the example built for wasm32, which
/// means a target CI does not install:
///
/// ```text
/// cd examples/autoreply && cargo build --release --target wasm32-unknown-unknown
/// cargo test -p oxidezap-plugin-host -- --ignored
/// ```
///
/// The `.wat` fixtures above test the host; this tests the *pair* — that what
/// `oxidezap-plugin` emits is what this file expects, which is the one thing
/// no hand-written module can check.
#[test]
#[ignore = "needs examples/autoreply built for wasm32-unknown-unknown"]
fn the_example_plugin_loads_and_answers_its_own_widgets() {
    let built = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/autoreply/target/wasm32-unknown-unknown/release/autoreply.wasm"
    );
    let bytes = std::fs::read(built).expect("build the example first; see this test's doc comment");

    let dir = TempDir::new("example");
    std::fs::write(dir.0.join("autoreply.wasm"), bytes).expect("writable");

    // Its own storage, because this plugin is off until somebody turns it on
    // and the toggle has to survive the call that flips it.
    let state = TempDir::new("example-state");
    let commands = Recorder::new(Outcome::Accepted);
    let published = Published::default();
    let plugins = Plugins::load(
        &dir.0,
        Some(&state.0),
        Arc::new(Arc::clone(&commands)),
        published.sink(),
    );

    // It draws its settings panel from `oxi_init`, before any event and
    // before anybody has allowed it anything — which is the point: this is
    // what the user reads before deciding.
    let surfaces = published.settles("its settings panel", |s| {
        s.first().is_some_and(|p| !p.roots.is_empty())
    });
    let surface = &surfaces[0];
    assert!(!surface.approved, "it has not been allowed to send yet");
    assert_eq!(surface.id, "autoreply");
    assert_eq!(surface.name, "Resposta automática");
    assert_eq!(
        surface.capabilities,
        vec![
            "send messages".to_string(),
            "add buttons and settings".into(),
            "keep its own settings".into(),
        ]
    );
    assert_eq!(surface.roots[0].slot, PluginSlot::Settings);
    let section = &surface.roots[0].node;
    assert_eq!(section.widget, PluginWidget::Section);
    assert!(!section.children[0].checked, "it starts switched off");

    plugins.approve("autoreply", true);
    published.settles("the answer", |s| s.first().is_some_and(|p| p.approved));

    // Its own toggle is still off, so it answers nothing.
    plugins.observe(&message("5511999@s.whatsapp.net", "ping"));
    std::thread::sleep(Duration::from_millis(200));
    assert!(commands.sent().is_empty());

    // Turn it on the way the window would, and it redraws itself.
    plugins.act(&PluginAction {
        plugin: "autoreply".into(),
        action: "enabled".into(),
        value: Some("1".into()),
        chat_jid: None,
        slot: PluginSlot::Settings,
    });
    published.settles("the toggle to come back on", |s| {
        s.first()
            .and_then(|p| p.roots.first())
            .is_some_and(|r| r.node.children[0].checked)
    });

    // Now the keyword matches and it replies, as a reply.
    plugins.observe(&message("5511999@s.whatsapp.net", "ping there"));
    until("the reply", || commands.sent().len() == 1);
    assert_eq!(
        commands.sent()[0],
        (
            "5511999@s.whatsapp.net".into(),
            "pong".into(),
            Some("MSG1".into())
        )
    );

    // A group is left alone, whatever it says.
    plugins.observe(&message("120363@g.us", "ping"));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(commands.sent().len(), 1, "groups are not answered");
}

/// A plugin that falls behind is stopped, and "stopped" has to mean it runs
/// no more events — not that it keeps working through a backlog while the
/// interface says it is off.
#[test]
fn a_plugin_stopped_for_falling_behind_is_offered_nothing_more() {
    let dir = TempDir::new("overflow");
    dir.plugin("autoreply", &pong());
    let commands = Recorder::new(Outcome::Accepted);
    let published = Published::default();
    let plugins = host(&dir, Arc::clone(&commands), &published);

    // Far more than the queue holds, faster than one wasm call each can be
    // made. The exact number that fits is not the point; the point is what
    // happens after it does not.
    for _ in 0..(QUEUE_DEPTH * 4) {
        plugins.observe(&message("a@s.whatsapp.net", "ping"));
    }

    let surfaces = published.settles("the plugin to be stopped", |s| {
        s.first().is_some_and(|p| !p.is_running())
    });
    assert!(
        surfaces[0]
            .stopped
            .as_deref()
            .is_some_and(|r| r.contains("behind")),
        "the reason should say what happened, got {:?}",
        surfaces[0].stopped
    );

    // It drains what it already had and then answers nothing further.
    std::thread::sleep(Duration::from_millis(300));
    let settled = commands.sent().len();
    for _ in 0..64 {
        plugins.observe(&message("a@s.whatsapp.net", "ping"));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(commands.sent().len(), settled, "it is not still working");
}

/// The snprintf contract, both halves: the answer is the value's *full*
/// length whether or not it fit, and exactly `min(cap, full)` bytes are
/// written — no fewer, so a caller can tell what its buffer holds.
#[test]
fn a_short_buffer_is_told_how_much_room_it_needed() {
    // Twelve bytes in ten characters, so the byte count and the character
    // count disagree — which is the whole reason this contract is stated in
    // bytes.
    const NEEDS_TWELVE: &str = "ação legal";
    let dir = TempDir::new("short-buffer");
    dir.plugin(
        "reader",
        &versioned(&format!(
            r#"(module
  (import "oxidezap" "oxi_subscribe" (func $subscribe (param i64)))
  (import "oxidezap" "oxi_request_caps" (func $caps (param i64)))
  (import "oxidezap" "oxi_field_str" (func $field_str (param i32 i32 i32 i32) (result i32)))
  (import "oxidezap" "oxi_send_text" (func $send (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "{jid}")
  (func (export "oxi_abi_version") (result i32) (i32.const $ABI_VERSION))
  (func (export "oxi_init") (result i32)
    (call $subscribe (i64.const 2))
    (call $caps (i64.const 1))
    (i32.const 0))
  (func (export "oxi_on_event") (param $kind i32) (param $ev i32) (result i32)
    (local $full i32)
    (local $written i32)
    ;; Ask for the text with room for 8 bytes only.
    (local.set $full (call $field_str (local.get $ev) (i32.const 11) (i32.const 512) (i32.const 8)))
    ;; Send back exactly what the buffer had room for, so the test can see
    ;; that the host wrote all eight bytes rather than stopping short of them.
    (local.set $written (i32.const 8))
    (drop (call $send (i32.const 0) (i32.const {jid_len}) (i32.const 512) (local.get $written)))
    ;; And a second send whose *length* is the full length the host answered.
    (drop (call $send (i32.const 0) (i32.const {jid_len}) (i32.const 512) (local.get $full)))
    (i32.const 0))
)"#,
            jid = "a@s.whatsapp.net",
            jid_len = "a@s.whatsapp.net".len()
        )),
    );

    let commands = Recorder::new(Outcome::Accepted);
    let published = Published::default();
    let plugins = host(&dir, Arc::clone(&commands), &published);
    plugins.observe(&message("a@s.whatsapp.net", NEEDS_TWELVE));
    until("both sends", || commands.sent().len() == 2);

    let sent = commands.sent();
    // The first is exactly the eight bytes the buffer had room for.
    assert_eq!(sent[0].1.len(), 8);
    assert_eq!(sent[0].1.as_bytes(), &NEEDS_TWELVE.as_bytes()[..8]);
    // The second is as long as the host said the whole value was, which is
    // the number a caller sizes its next buffer from.
    assert_eq!(sent[1].1.len(), NEEDS_TWELVE.len());
}

// ---- what a plugin may do, and when ------------------------------------

/// The gate that makes the permission sentence mean something: copying a
/// `.wasm` into a folder grants nothing, and until somebody agrees the plugin
/// runs and is refused.
#[test]
fn a_plugin_cannot_act_on_the_account_until_it_is_allowed() {
    let dir = TempDir::new("ungranted");
    dir.plugin("autoreply", &pong());
    let commands = Recorder::new(Outcome::Accepted);
    let published = Published::default();
    let plugins = unapproved_host(&dir, Arc::clone(&commands), &published);

    let surfaces = published.settles("the plugin to be listed", |s| !s.is_empty());
    assert!(!surfaces[0].approved, "it is waiting on an answer");
    assert_eq!(surfaces[0].capabilities, vec!["send messages".to_string()]);

    plugins.observe(&message("a@s.whatsapp.net", "ping"));
    std::thread::sleep(Duration::from_millis(250));
    assert!(commands.sent().is_empty(), "asking is not being allowed");
    assert!(
        surfaces[0].is_running(),
        "and it is refused rather than stopped: it may still watch"
    );

    // Now say yes, and the very next message is answered.
    plugins.approve("autoreply", true);
    published.settles("the answer to be recorded", |s| {
        s.first().is_some_and(|p| p.approved)
    });
    plugins.observe(&message("a@s.whatsapp.net", "ping"));
    until("the reply", || commands.sent().len() == 1);

    // And taking it back stops it again.
    plugins.approve("autoreply", false);
    published.settles("the withdrawal", |s| s.first().is_some_and(|p| !p.approved));
    plugins.observe(&message("a@s.whatsapp.net", "ping"));
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(commands.sent().len(), 1, "nothing more went out");
}

/// Drawing is not gated, and it must not be: a plugin that could not publish
/// its settings panel before being allowed would leave the user agreeing to a
/// name and a list of phrases with nothing to look at.
#[test]
fn an_unapproved_plugin_can_still_explain_itself() {
    let dir = TempDir::new("draws-unapproved");
    dir.plugin("greeter", &draws());
    let published = Published::default();
    let _plugins = unapproved_host(&dir, Recorder::new(Outcome::Accepted), &published);

    let surfaces = published.settles("its interface", |s| {
        s.first().is_some_and(|p| !p.roots.is_empty())
    });
    assert!(!surfaces[0].approved, "it still cannot send");
    assert_eq!(surfaces[0].name, NAME, "and it still says who it is");
}

/// Shutdown has to finish from any state, and the hard one is a plugin whose
/// queue is full: a stop message would have nowhere to go, so the sender is
/// dropped and a flag tells the worker to abandon its backlog.
#[test]
fn shutting_down_finishes_even_with_a_saturated_queue() {
    let dir = TempDir::new("saturated");
    dir.plugin("autoreply", &pong());
    let plugins = host(
        &dir,
        Recorder::new(Outcome::Accepted),
        &Published::default(),
    );

    for _ in 0..(QUEUE_DEPTH * 4) {
        plugins.observe(&message("a@s.whatsapp.net", "ping"));
    }

    // On another thread with a deadline, because the failure this guards
    // against is a hang: an assertion that never runs proves nothing, and a
    // test that hangs takes CI's whole timeout with it.
    //
    // Detached, and that is the whole point: `thread::scope` joins its
    // threads before it returns *or* propagates a panic, so a `shutdown`
    // that hung would have the timeout fire and then wait forever anyway —
    // the deadline could never report the one failure it exists to catch.
    let (done, waited) = std::sync::mpsc::channel();
    let plugins = Arc::new(plugins);
    let shutting_down = Arc::clone(&plugins);
    std::thread::spawn(move || {
        shutting_down.shutdown();
        let _ = done.send(());
    });
    waited
        .recv_timeout(Duration::from_secs(20))
        .expect("shutdown returned");
}

/// `MAX_TIMERS` bounds what a plugin *holds*, not what it asks for in one
/// call — otherwise a few far-future timers per message would grow the
/// worker's list forever.
#[test]
fn a_plugin_cannot_grow_its_timer_list_across_calls() {
    let dir = TempDir::new("timers");
    dir.plugin("greedy", &arms_timers());
    let commands = Recorder::new(Outcome::Accepted);
    let published = Published::default();
    let plugins = host(&dir, Arc::clone(&commands), &published);

    // Each message asks for four timers, an hour out so none of them fires.
    // Past the cap the host refuses, and the plugin reports the refusal by
    // sending a message naming what it was told.
    for _ in 0..12 {
        plugins.observe(&message("a@s.whatsapp.net", "tick"));
    }
    until("a refusal", || {
        commands.sent().iter().any(|(_, text, _)| text == "refused")
    });
}

/// Asks for one timer at the far end of the `i64` and says which answer it
/// got, so the test reads the host's refusal rather than inferring it.
const ARMS_AN_ETERNAL_TIMER: &str = r#"(module
  (import "oxidezap" "oxi_subscribe"    (func $subscribe (param i64)))
  (import "oxidezap" "oxi_request_caps" (func $caps (param i64)))
  (import "oxidezap" "oxi_timer_set"    (func $timer (param i64 i64) (result i32)))
  (import "oxidezap" "oxi_send_text"    (func $send (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "a@s.whatsapp.net")
  (data (i32.const 64) "refusedarmed")
  (func (export "oxi_abi_version") (result i32) (i32.const $ABI_VERSION))
  (func (export "oxi_init") (result i32)
    (call $subscribe (i64.const 2))         ;; messages
    (call $caps (i64.const 33))             ;; caps::SEND | caps::TIMERS
    (i32.const 0))
  (func (export "oxi_on_event") (param $kind i32) (param $ev i32) (result i32)
    (if (i32.lt_s (call $timer (i64.const 9223372036854775807) (i64.const 1)) (i32.const 0))
      (then (drop (call $send (i32.const 0) (i32.const 16) (i32.const 64) (i32.const 7))))
      (else (drop (call $send (i32.const 0) (i32.const 16) (i32.const 71) (i32.const 5)))))
    (i32.const 0))
)"#;

/// A delay past the ceiling is refused rather than clamped or armed.
///
/// `i64::MAX` milliseconds is a quarter of a billion years, which saturates
/// the monotonic clock it would be added to: the timer never comes due and
/// holds one of the sixteen a plugin may have for the life of the process. A
/// plugin that disarms itself by an arithmetic mistake is told about it.
#[test]
fn a_timer_past_the_far_end_of_time_is_refused() {
    let dir = TempDir::new("eternal-timer");
    dir.plugin("patient", &versioned(ARMS_AN_ETERNAL_TIMER));
    let commands = Recorder::new(Outcome::Accepted);
    let published = Published::default();
    let plugins = host(&dir, Arc::clone(&commands), &published);

    plugins.observe(&message("a@s.whatsapp.net", "tick"));
    until("an answer", || !commands.sent().is_empty());
    assert_eq!(
        commands.sent()[0].1,
        "refused",
        "a delay no clock can represent is not a timer"
    );
}

/// A plugin that wants nothing but to draw: no account capability at all.
fn draws_only() -> String {
    let mut buf = vec![0u8; 512];
    let mut w = abi::ui::Writer::new(&mut buf);
    w.leaf(
        abi::ui::kind::LABEL,
        abi::ui::slot::SETTINGS,
        abi::ui::flags::ENABLED,
        "",
        "Nada a pedir.",
        "",
    );
    let n = w.finish().expect("fits");
    versioned(&format!(
        r#"(module
  (import "oxidezap" "oxi_request_caps" (func $caps (param i64)))
  (import "oxidezap" "oxi_ui_set"       (func $ui_set (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "{tree}")
  (func (export "oxi_abi_version") (result i32) (i32.const $ABI_VERSION))
  (func (export "oxi_init") (result i32)
    (call $caps (i64.const 8))    ;; caps::UI, and nothing that reaches the account
    (drop (call $ui_set (i32.const 0) (i32.const {len})))
    (i32.const 0))
  (func (export "oxi_on_event") (param i32) (param i32) (result i32) (i32.const 0))
)"#,
        tree = wat_bytes(&buf[..n]),
        len = n,
    ))
}

/// A module that acts on the account from its *start section* — before the
/// loader has read back its ABI version or checked its exports.
const ACTS_WHILE_LOADING: &str = r#"(module
  (import "oxidezap" "oxi_request_caps" (func $caps (param i64)))
  (import "oxidezap" "oxi_send_text"    (func $send (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "a@s.whatsapp.netsneaked")
  (func $start
    (call $caps (i64.const 1))
    (drop (call $send (i32.const 0) (i32.const 16) (i32.const 16) (i32.const 7))))
  (start $start)
  (func (export "oxi_abi_version") (result i32) (i32.const $ABI_VERSION))
  (func (export "oxi_init") (result i32) (i32.const 0))
  (func (export "oxi_on_event") (param i32) (param i32) (result i32) (i32.const 0))
)"#;

/// The one moment a plugin runs before the host has accepted it. A start
/// section, or a side-effecting `oxi_abi_version`, is code the loader has not
/// agreed to run — and a module it goes on to refuse would otherwise have
/// sent a message on its way out the door.
#[test]
fn a_module_cannot_act_on_the_account_before_it_is_loaded() {
    let dir = TempDir::new("start-section");
    dir.plugin("sneaky", &versioned(ACTS_WHILE_LOADING));
    let commands = Recorder::new(Outcome::Accepted);
    let published = Published::default();
    // Approved in advance, which is the hard case: the answer is on file, so
    // nothing but the phase stands between this module and the account.
    let _plugins = host(&dir, Arc::clone(&commands), &published);

    std::thread::sleep(Duration::from_millis(250));
    assert!(
        commands.sent().is_empty(),
        "its start section ran with every import refusing"
    );
}

/// Two declarations, the second wider than the first — with an account action
/// in between. The all-or-nothing rule is about a sentence, and a plugin that
/// acts under a narrow one before widening it has already acted.
const REDECLARES: &str = r#"(module
  (import "oxidezap" "oxi_request_caps" (func $caps (param i64)))
  (import "oxidezap" "oxi_send_text"    (func $send (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "a@s.whatsapp.netsneaked")
  (func (export "oxi_abi_version") (result i32) (i32.const $ABI_VERSION))
  (func (export "oxi_init") (result i32)
    (call $caps (i64.const 1))        ;; SEND, the mask it was approved for
    (drop (call $send (i32.const 0) (i32.const 16) (i32.const 16) (i32.const 7)))
    (call $caps (i64.const 3))        ;; and now SEND | MARK_READ
    (i32.const 0))
  (func (export "oxi_on_event") (param i32) (param i32) (result i32) (i32.const 0))
)"#;

#[test]
fn a_plugin_declares_what_it_wants_exactly_once() {
    let dir = TempDir::new("redeclares");
    dir.plugin("shifty", &versioned(REDECLARES));
    dir.plugin("autoreply", &pong());
    let commands = Recorder::new(Outcome::Accepted);
    let published = Published::default();
    let plugins = host(&dir, Arc::clone(&commands), &published);

    // Refused rather than loaded under the first sentence. Ignoring the
    // second used to be enough — the widened half never took effect — but the
    // import answers nothing, so a plugin that declared twice by accident (two
    // helpers, each declaring its own mask) ran with half of what it wrote,
    // showed that half in Settings, and had the rest denied for good with
    // nothing anywhere saying why.
    assert_eq!(
        plugins.ids(),
        vec!["autoreply"],
        "a second declaration is not a correction, it is a different sentence"
    );
    assert!(
        commands.sent().is_empty(),
        "and what it sent between the two declarations went nowhere"
    );
    // The fixture is otherwise loadable, so this test is about the second
    // declaration rather than about anything else being wrong with it.
    let _ = published;
}

/// A plugin whose settings and buttons are its own business has nothing to
/// consent to — and a switch over it could be turned off and would read as on
/// again the moment it was drawn.
#[test]
fn a_plugin_that_only_draws_has_nothing_to_agree_to() {
    let dir = TempDir::new("self-only");
    dir.plugin("greeter", &draws_only());
    let published = Published::default();
    let _plugins = unapproved_host(&dir, Recorder::new(Outcome::Accepted), &published);

    let surfaces = published.settles("its interface", |s| {
        s.first().is_some_and(|p| !p.roots.is_empty())
    });
    assert!(surfaces[0].gated.is_empty(), "nothing touches the account");
    assert!(surfaces[0].approved, "so there is no prompt to answer");
    assert!(
        !surfaces[0].capabilities.is_empty(),
        "what it does is still said, as information rather than as a question"
    );
}

/// Withdrawing has to take effect *now*, not once the plugin has worked
/// through what it was already handed. A queued answer would leave it sending
/// through every banked event while Settings already read "not allowed" — and
/// the plugin that most needs its permissions taken away is exactly the one
/// with a full queue, where a queued answer would not fit at all.
#[test]
fn withdrawing_stops_a_plugin_part_way_through_its_backlog() {
    let dir = TempDir::new("revoke-mid-backlog");
    dir.plugin("autoreply", &pong());
    let commands = Recorder::new(Outcome::Accepted);
    let published = Published::default();
    let plugins = host(&dir, Arc::clone(&commands), &published);

    // Park its thread inside the first command, then bank four more events
    // behind it.
    commands.close_gate();
    for _ in 0..5 {
        plugins.observe(&message("a@s.whatsapp.net", "ping"));
    }
    std::thread::sleep(Duration::from_millis(150));

    plugins.approve("autoreply", false);
    commands.open_gate();

    // The one already inside the command completes; nothing behind it does.
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        commands.sent().len(),
        1,
        "the backlog drained with the answer already applied"
    );
}

/// A handle clones its string into the *daemon's* memory, which wasmi's
/// limiter does not bound. Without a cap a plugin can spend its fuel budget
/// asking for the same list element over and over and grow the host far past
/// the memory its sandbox advertises.
const HOARDS_HANDLES: &str = r#"(module
  (import "oxidezap" "oxi_subscribe" (func $subscribe (param i64)))
  (import "oxidezap" "oxi_field_at"  (func $field_at (param i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "oxi_abi_version") (result i32) (i32.const $ABI_VERSION))
  (global $last (mut i32) (i32.const 0))
  (func (export "oxi_init") (result i32)
    (call $subscribe (i64.const 2))
    (i32.const 0))
  (func (export "oxi_on_event") (param $kind i32) (param $ev i32) (result i32)
    (local $i i32)
    (block $done
      (loop $again
        (br_if $done (i32.ge_s (local.get $i) (i32.const 100000)))
        ;; fields::MESSAGE_ID is not a list, so this is the honest shape of
        ;; the attack against whatever field is: ask, and ask again.
        (global.set $last (call $field_at (local.get $ev) (i32.const 10) (i32.const 0)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $again)))
    (i32.const 0))
  (func (export "last") (result i32) (global.get $last))
)"#;

#[test]
fn a_plugin_cannot_hoard_handles_out_of_the_hosts_memory() {
    let dir = TempDir::new("handles");
    dir.plugin("greedy", &versioned(HOARDS_HANDLES));
    let published = Published::default();
    let plugins = host(&dir, Recorder::new(Outcome::Accepted), &published);

    plugins.observe(&message("a@s.whatsapp.net", "ping"));
    // Whatever it ends up doing, it does not grow the daemon: either the
    // arena refuses it or fuel does. What must not happen is a hundred
    // thousand strings landing in host memory.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !published.latest().is_empty(),
        "the daemon is still here to say so"
    );
}

/// The bytes of a module, and everything wasmi allocates parsing them, are
/// spent before the store — and so before its limiter — exists.
#[test]
fn a_module_too_large_to_be_a_plugin_is_refused_unread() {
    let dir = TempDir::new("oversized");
    let path = dir.0.join("huge.wasm");
    std::fs::create_dir_all(&dir.0).expect("a writable temp dir");
    // A file past the cap, cheaply: the loader must answer from its size
    // rather than from its contents.
    let file = std::fs::File::create(&path).expect("writable");
    file.set_len(crate::MAX_MODULE_BYTES as u64 + 1)
        .expect("sparse");
    drop(file);

    // A good plugin beside it, so an empty list cannot pass this test by
    // discovery having failed altogether.
    dir.plugin("autoreply", &pong());

    let published = Published::default();
    let plugins = unapproved_host(&dir, Recorder::new(Outcome::Accepted), &published);
    assert_eq!(
        plugins.ids(),
        vec!["autoreply"],
        "the oversized one is refused, and its neighbour still loads"
    );
}

/// A plugin's own key-value file and the host's record of what the user
/// allowed live in the same directory, so no plugin id may name the latter.
#[test]
fn a_plugin_cannot_name_the_file_that_holds_its_own_approval() {
    let dir = TempDir::new("approvals-collision");
    let state = dir.0.join("state");
    std::fs::create_dir_all(&state).expect("writable");

    let approvals = crate::approvals::Approvals::open(Some(&state));
    approvals.set("victim", abi::caps::SEND, true);

    // The worst case: a plugin actually called `approvals`.
    let mut kv = crate::kv::Kv::open(&state, "approvals");
    kv.set("anything", "at all");

    let reread = crate::approvals::Approvals::open(Some(&state));
    assert_eq!(
        reread.approved("victim"),
        abi::caps::SEND,
        "a plugin's settings did not land on everybody's permissions"
    );
}

/// The filter that decides whether an event is worth building must admit
/// exactly the events the conversion handles. Two matches that disagree is
/// the one way this optimisation becomes a plugin silently missing events.
#[test]
fn every_converted_event_is_one_the_filter_admits() {
    // Every variant `kind_of` admits, because a case it omits is one where
    // the two matches may drift and a plugin silently stops being told.
    let cases: Vec<UiEvent> = vec![
        message("a@s.whatsapp.net", "hi"),
        UiEvent::Connected,
        UiEvent::Disconnected("because".into()),
        UiEvent::LoggedOut("because".into()),
        UiEvent::QrCode {
            code: "q".into(),
            timeout_secs: 60,
        },
        UiEvent::PairCode {
            code: "p".into(),
            timeout_secs: 60,
        },
        UiEvent::PairSuccess,
        UiEvent::ReceiptReceived {
            chat_jid: "a@s.whatsapp.net".into(),
            message_ids: vec!["MSG1".into()],
            receipt_type: oxidezap_core::ReceiptType::Read,
        },
        UiEvent::ReactionReceived {
            chat_jid: "a@s.whatsapp.net".into(),
            message_id: "MSG1".into(),
            sender: "a@s.whatsapp.net".into(),
            emoji: "\u{1f44d}".into(),
        },
        UiEvent::ChatPresence {
            chat_jid: "a@s.whatsapp.net".into(),
            sender_jid: "a@s.whatsapp.net".into(),
            sender_name: None,
            composing: None,
        },
        UiEvent::CallAnswered {
            call_id: "c1".into(),
            is_video: false,
        },
        UiEvent::CallEnded("c1".into()),
        UiEvent::CallEndedElsewhere("c1".into()),
        UiEvent::CallAccepted("c1".into()),
    ];
    for case in cases {
        assert_eq!(
            crate::event::kind_of(&case),
            crate::event::from_session(&case).map(|e| e.kind),
            "the filter and the conversion disagree about {case:?}"
        );
    }
}

/// A call the peer accepted is a call that was answered. Without it a plugin
/// watching an outgoing call sees it start and end with nothing in between,
/// and cannot tell one that connected from one nobody picked up.
#[test]
fn a_peer_accepting_our_call_reaches_a_plugin_as_answered() {
    let event = crate::event::from_session(&UiEvent::CallAccepted("c1".into()))
        .expect("a call event a plugin can act on");
    assert_eq!(event.kind, abi::kinds::CALL);
    assert_eq!(
        event.get(abi::fields::CALL_EVENT),
        Some(&crate::event::Value::Int(abi::fields::call::ANSWERED))
    );
}

/// An id is what an approval, a settings file and an action are keyed on, so
/// two files claiming one are two plugins sharing an identity — and taking a
/// permission away would reach one of them while the other kept its own copy
/// of the mask.
#[test]
fn two_files_cannot_claim_one_plugin_id() {
    let dir = TempDir::new("same-id");
    dir.plugin("autoreply", &pong());
    // The same stem with the extension in another case: two files on a
    // case-sensitive filesystem, one id here.
    std::fs::write(
        dir.0.join("autoreply.WASM"),
        wat::parse_str(pong()).expect("valid"),
    )
    .expect("writable");
    // Two files, or there is nothing here to guard against. On a
    // case-insensitive filesystem — macOS and Windows, both of which CI runs
    // — that second write replaced the first, and one id from one file proves
    // nothing. The collision cannot be built any other way: `plugin_id` does
    // not fold case, so `Foo.wasm` and `foo.wasm` are already two ids.
    if std::fs::read_dir(&dir.0).expect("readable").count() < 2 {
        return;
    }

    let published = Published::default();
    let plugins = unapproved_host(&dir, Recorder::new(Outcome::Accepted), &published);
    assert_eq!(plugins.ids(), vec!["autoreply"], "one of them, not two");
}

/// A plugin's settings are written when its call returns, not on every `set`.
/// Fuel does not price a rename, so a write per key is filesystem I/O a
/// plugin can ask for without limit.
const WRITES_A_LOT: &str = r#"(module
  (import "oxidezap" "oxi_subscribe"   (func $subscribe (param i64)))
  (import "oxidezap" "oxi_request_caps" (func $caps (param i64)))
  (import "oxidezap" "oxi_kv_set"      (func $set (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "k01")
  (func (export "oxi_abi_version") (result i32) (i32.const $ABI_VERSION))
  (func (export "oxi_init") (result i32)
    (call $subscribe (i64.const 2))
    (call $caps (i64.const 16))   ;; caps::STORAGE
    (i32.const 0))
  (func (export "oxi_on_event") (param $kind i32) (param $ev i32) (result i32)
    (local $i i32)
    (block $done
      (loop $again
        (br_if $done (i32.ge_s (local.get $i) (i32.const 500)))
        ;; The same key, alternating between two one-byte values.
        (i32.store8 (i32.const 8) (i32.add (i32.const 48) (i32.rem_s (local.get $i) (i32.const 2))))
        (drop (call $set (i32.const 0) (i32.const 1) (i32.const 8) (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $again)))
    (i32.const 0))
)"#;

/// The same bound through the real path: a module hammering one key inside a
/// single call survives it, and what it wrote is on disk once the call has
/// returned.
#[test]
fn a_plugin_hammering_its_store_costs_one_write_and_keeps_running() {
    let dir = TempDir::new("kv-hammer");
    dir.plugin("busy", &versioned(WRITES_A_LOT));
    let state = dir.0.join("state");
    let published = Published::default();
    let plugins = Plugins::load(
        &dir.0,
        Some(&state),
        Arc::new(Recorder::new(Outcome::Accepted)),
        published.sink(),
    );

    plugins.observe(&message("a@s.whatsapp.net", "go"));
    let surfaces = published.settles("the plugin to be listed", |s| !s.is_empty());
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        surfaces[0].is_running(),
        "five hundred sets in one call is not a reason to stop it"
    );
    assert!(
        state.join("kv-busy.json").exists(),
        "and the call returning wrote what it kept"
    );
}

#[test]
fn a_plugins_settings_are_written_once_a_call_rather_than_once_a_key() {
    let dir = TempDir::new("kv-writes");
    let state = dir.0.join("state");
    std::fs::create_dir_all(&state).expect("writable");

    let mut kv = crate::kv::Kv::open(&state, "busy");
    for i in 0..500 {
        kv.set("k", if i % 2 == 0 { "0" } else { "1" });
    }
    assert!(
        !state.join("kv-busy.json").exists(),
        "five hundred sets have written nothing yet"
    );
    kv.commit();
    assert!(
        state.join("kv-busy.json").exists(),
        "and the call returning writes them once"
    );
}

/// The rule itself: a directory the host cannot make private is refused,
/// and refused *before* anything is read out of it.
///
/// Its ordering — securing before reading — is the half a test cannot reach:
/// forcing a `chmod` to fail on a directory this process owns needs another
/// user, which a unit test does not have. What is checked here is the answer
/// the ordering exists to give.
#[test]
fn a_directory_that_cannot_be_made_private_is_refused() {
    let dir = TempDir::new("usable-state");
    assert_eq!(crate::usable_state_dir(None), None, "nowhere to keep it");

    let good = dir.0.join("state");
    assert_eq!(
        crate::usable_state_dir(Some(&good)),
        Some(good.as_path()),
        "a directory it can create and lock down is its own"
    );

    // A file where the directory would go: it cannot be created, so it
    // cannot be made private, so it is not used.
    let blocker = dir.0.join("blocked");
    std::fs::write(&blocker, b"not a directory").expect("writable");
    assert_eq!(crate::usable_state_dir(Some(&blocker.join("state"))), None);
}

/// Names itself twice and reports what the host answered the second time.
const RENAMES_ITSELF: &str = r#"(module
  (import "oxidezap" "oxi_request_caps" (func $caps (param i64)))
  (import "oxidezap" "oxi_set_name"     (func $set_name (param i32 i32) (result i32)))
  (import "oxidezap" "oxi_send_text"    (func $send (param i32 i32 i32 i32) (result i32)))
  (import "oxidezap" "oxi_subscribe"    (func $subscribe (param i64)))
  (memory (export "memory") 1)
  (data (i32.const 0) "a@s.whatsapp.netPrimeiroSegundorefused")
  (func (export "oxi_abi_version") (result i32) (i32.const $ABI_VERSION))
  (func (export "oxi_init") (result i32)
    (call $subscribe (i64.const 2))
    (call $caps (i64.const 1))    ;; caps::SEND
    (drop (call $set_name (i32.const 16) (i32.const 8)))     ;; "Primeiro"
    (global.set $answer (call $set_name (i32.const 24) (i32.const 7)))  ;; "Segundo"
    (i32.const 0))
  (global $answer (mut i32) (i32.const 0))
  (func (export "oxi_on_event") (param $kind i32) (param $ev i32) (result i32)
    (if (i32.lt_s (global.get $answer) (i32.const 0))
      (then (drop (call $send (i32.const 0) (i32.const 16) (i32.const 31) (i32.const 7)))))
    (i32.const 0))
)"#;

/// A plugin names itself once.
///
/// The second call is refused *before* the string is read, which is the
/// point: answering it meant a kilobyte out of guest memory and an
/// allocation per call, priced as one fixed-cost import, with `oxi_init`
/// carrying two hundred million fuel and the daemon's startup waiting.
#[test]
fn a_plugin_names_itself_once() {
    let dir = TempDir::new("renames");
    dir.plugin("shifty", &versioned(RENAMES_ITSELF));
    let commands = Recorder::new(Outcome::Accepted);
    let published = Published::default();
    let plugins = host(&dir, Arc::clone(&commands), &published);

    let surfaces = published.settles("the plugin to be listed", |s| !s.is_empty());
    assert_eq!(
        surfaces[0].name, "Primeiro",
        "the first name is the one it has"
    );

    // And it was told, rather than left to wonder why its name never changed.
    plugins.observe(&message("a@s.whatsapp.net", "oi"));
    until("the report", || {
        commands.sent().iter().any(|(_, text, _)| text == "refused")
    });
}

/// A state directory that cannot be made private is not used at all.
///
/// `approvals.json` says what each plugin may do to the account, so a
/// directory another local user can write is one where that file says what
/// somebody else decided. Fails closed: nothing is read from it, nothing is
/// written to it, and the plugin runs unapproved.
#[test]
fn a_state_directory_that_cannot_be_secured_is_not_used() {
    let dir = TempDir::new("unsecurable-state");
    dir.plugin("autoreply", &pong());
    // A file where the directory would go, so creating it cannot succeed —
    // the same answer the host must give a directory it cannot chmod.
    let blocker = dir.0.join("blocked");
    std::fs::write(&blocker, b"not a directory").expect("writable");
    let state = blocker.join("state");

    let commands = Recorder::new(Outcome::Accepted);
    let published = Published::default();
    let plugins = Plugins::load(
        &dir.0,
        Some(&state),
        Arc::new(Arc::clone(&commands)),
        published.sink(),
    );

    let surfaces = published.settles("the plugin to be listed", |s| !s.is_empty());
    assert!(
        surfaces[0].is_running(),
        "the plugin still runs: it can draw and it can keep settings in memory"
    );
    assert!(
        !surfaces[0].approved,
        "but nothing it does to the account is allowed on an approval file this \
         daemon could not vouch for"
    );

    // And it stays refused in practice, not only in the surface.
    plugins.observe(&message("a@s.whatsapp.net", "ping"));
    std::thread::sleep(Duration::from_millis(200));
    assert!(commands.sent().is_empty(), "no send went out");
}

/// A debt outlives the window it was run up in.
///
/// The rule is asked as a question about a length of time rather than read
/// off a clock, which is what makes this checkable: an eleven-second window
/// is not something a test can wait for, and a wasm call is fuel-bounded and
/// cannot be made to last ten seconds on demand.
#[test]
fn time_owed_is_not_forgiven_when_the_window_turns_over() {
    use crate::Turn;

    // 1.2 seconds of running, which at a tenth wants twelve seconds of
    // window. Eleven have passed — the window is over, and a second is still
    // owed. Rolling it over here is the forgiveness this exists to refuse.
    let over = crate::Duty {
        window_began: wacore::time::Instant::now(),
        busy: Duration::from_millis(1_200),
    };
    assert_eq!(
        over.decide(Duration::from_secs(11)),
        Turn::Wait(Duration::from_secs(1))
    );

    // The same plugin once it has paid: the window is up and may start again.
    assert_eq!(over.decide(Duration::from_secs(12)), Turn::Roll);

    // And inside a window it has not filled, it simply runs.
    assert_eq!(
        over.decide(Duration::from_secs(5)),
        Turn::Wait(Duration::from_secs(7))
    );
    let idle = crate::Duty {
        window_began: wacore::time::Instant::now(),
        busy: Duration::from_millis(100),
    };
    assert_eq!(idle.decide(Duration::from_secs(5)), Turn::Go);

    // A debt larger than a window is paid a window at a time rather than in
    // one sleep the daemon would have to wait out to shut down.
    let stalled = crate::Duty {
        window_began: wacore::time::Instant::now(),
        busy: Duration::from_secs(30),
    };
    // A debt is waited out whole rather than truncated at a window. Capping
    // it looked like the careful answer and was a way out of the share: a
    // plugin that slept a window and then ran another long callback gained
    // debt faster than it paid it. Nothing is lost by waiting — the sleep is
    // taken in slices, so a plugin held back for minutes is still one the
    // daemon joins in milliseconds.
    assert_eq!(
        stalled.decide(Duration::from_secs(30)),
        Turn::Wait(Duration::from_secs(270))
    );
}

/// A plugin another local account could replace is not loaded.
///
/// The approval is recorded against a plugin's id and its mask rather than
/// its bytes — deliberately, so an update does not ask again — which is
/// exactly what makes a writable file dangerous: somebody else's
/// `autoreply.wasm` under that name inherits whatever this account once
/// agreed to.
#[cfg(unix)]
#[test]
fn a_plugin_anyone_could_have_replaced_is_not_loaded() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = TempDir::new("writable");
    dir.plugin("autoreply", &pong());
    dir.plugin("exposed", &pong());
    let exposed = dir.0.join("exposed.wasm");
    std::fs::set_permissions(&exposed, std::fs::Permissions::from_mode(0o666)).expect("chmod");

    let published = Published::default();
    let plugins = unapproved_host(&dir, Recorder::new(Outcome::Accepted), &published);
    assert_eq!(
        plugins.ids(),
        vec!["autoreply"],
        "the world-writable one is skipped and the one beside it still loads"
    );

    // And a directory anybody can write takes everything with it: a new name
    // can appear there too, not only new bytes under an old one.
    std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o777)).expect("chmod");
    let published = Published::default();
    let plugins = unapproved_host(&dir, Recorder::new(Outcome::Accepted), &published);
    assert!(plugins.is_empty());
    // Put it back, or the directory cannot be removed cleanly.
    std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// Every bound in the host is per plugin; the count is the bound on the sum.
/// A directory holding more than the daemon will run costs it nothing past
/// the limit — not a thread, not a queue, not a wasmi store.
#[test]
fn a_directory_of_plugins_is_bounded_by_how_many_will_run() {
    let dir = TempDir::new("too-many");
    let wasm = pong();
    for i in 0..crate::MAX_PLUGINS + 4 {
        dir.plugin(&format!("p{i:03}"), &wasm);
    }

    let published = Published::default();
    let plugins = unapproved_host(&dir, Recorder::new(Outcome::Accepted), &published);
    assert_eq!(plugins.ids().len(), crate::MAX_PLUGINS);
}

/// Everything the plugin host keeps is account-scoped, and the store it
/// belongs beside is under `%LOCALAPPDATA%`. A roaming profile would carry an
/// approval to a machine holding a different account.
#[cfg(target_os = "windows")]
#[test]
fn windows_state_does_not_roam() {
    let local = std::env::var_os("LOCALAPPDATA").expect("windows sets this");
    let state = crate::default_state_dir().expect("a state dir");
    assert!(
        state.starts_with(std::path::Path::new(&local)),
        "{} is not under %LOCALAPPDATA%",
        state.display()
    );
}

/// A plugin's store holds whatever it kept, and an autoreply's list of who it
/// has answered is a list of people. Per-user state means this user.
#[cfg(unix)]
#[test]
fn a_plugins_stored_settings_are_not_readable_by_other_users() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = TempDir::new("private");
    let state = dir.0.join("state");
    crate::create_private_dir(&state).expect("writable");
    assert_eq!(
        std::fs::metadata(&state)
            .expect("it exists")
            .permissions()
            .mode()
            & 0o777,
        0o700,
        "nobody else may even enter it"
    );

    let mut kv = crate::kv::Kv::open(&state, "p");
    kv.set("who", "somebody");
    kv.commit();
    assert_eq!(
        std::fs::metadata(state.join("kv-p.json"))
            .expect("written")
            .permissions()
            .mode()
            & 0o777,
        0o600,
        "and the file itself is owner-only"
    );
}

/// Publishing a tree is copying, parsing and allocating — host work the
/// sandbox does not measure, since only the instructions around the import
/// burn fuel. A plugin calling it in a loop must run out of *this* rather
/// than out of the daemon's memory.
#[test]
fn a_plugin_cannot_publish_an_unbounded_number_of_trees_in_one_call() {
    let dir = TempDir::new("ui-flood");
    dir.plugin("noisy", &publishes_repeatedly());
    let published = Published::default();
    let plugins = host(&dir, Recorder::new(Outcome::Accepted), &published);

    // Its tree reaches the daemon, so the first publishes were taken.
    let surfaces = published.settles("its interface", |s| {
        s.first().is_some_and(|p| !p.roots.is_empty())
    });
    assert!(surfaces[0].is_running());

    plugins.observe(&message("a@s.whatsapp.net", "go"));
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        published
            .latest()
            .first()
            .is_some_and(oxidezap_core::PluginSurface::is_running),
        "the module traps unless the cap refused it, so surviving is the proof"
    );
}

/// A tree published over and over inside one call, each one valid.
fn publishes_repeatedly() -> String {
    let mut buf = vec![0u8; 512];
    let mut w = abi::ui::Writer::new(&mut buf);
    w.leaf(
        abi::ui::kind::LABEL,
        abi::ui::slot::SETTINGS,
        abi::ui::flags::ENABLED,
        "",
        "oi",
        "",
    );
    let n = w.finish().expect("fits");
    versioned(&format!(
        r#"(module
  (import "oxidezap" "oxi_subscribe"    (func $subscribe (param i64)))
  (import "oxidezap" "oxi_request_caps" (func $caps (param i64)))
  (import "oxidezap" "oxi_ui_set"       (func $ui_set (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "{tree}")
  (func (export "oxi_abi_version") (result i32) (i32.const $ABI_VERSION))
  (global $refusals (mut i32) (i32.const 0))
  (func (export "oxi_init") (result i32)
    (call $subscribe (i64.const 2))
    (call $caps (i64.const 8))   ;; caps::UI
    (drop (call $ui_set (i32.const 0) (i32.const {len})))
    (i32.const 0))
  (func (export "oxi_on_event") (param $kind i32) (param $ev i32) (result i32)
    (local $i i32)
    (block $done
      (loop $again
        (br_if $done (i32.ge_s (local.get $i) (i32.const 1000)))
        (if (i32.ne (call $ui_set (i32.const 0) (i32.const {len})) (i32.const 0))
          (then (global.set $refusals (i32.add (global.get $refusals) (i32.const 1)))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $again)))
    ;; The assertion, made from inside: if the cap never engaged, this traps
    ;; and the host stops the plugin — so the test's "still running" below is
    ;; evidence that a thousand publishes were refused rather than evidence
    ;; that nothing interesting happened.
    (if (i32.eqz (global.get $refusals)) (then (unreachable)))
    (i32.const 0))
)"#,
        tree = wat_bytes(&buf[..n]),
        len = n,
    ))
}

/// A module without a memory export loads and then answers `INVALID` to
/// everything, which reaches the user as a plugin listed as running whose
/// controls quietly do nothing. Refused at load, where the reason can be said.
const NO_MEMORY: &str = r#"(module
  (func (export "oxi_abi_version") (result i32) (i32.const $ABI_VERSION))
  (func (export "oxi_init") (result i32) (i32.const 0))
  (func (export "oxi_on_event") (param i32) (param i32) (result i32) (i32.const 0))
)"#;

#[test]
fn a_module_with_nothing_to_read_from_is_refused() {
    let dir = TempDir::new("no-memory");
    dir.plugin("mute", &versioned(NO_MEMORY));
    dir.plugin("autoreply", &pong());

    let published = Published::default();
    let plugins = unapproved_host(&dir, Recorder::new(Outcome::Accepted), &published);
    assert_eq!(
        plugins.ids(),
        vec!["autoreply"],
        "refused, and its neighbour still loads"
    );
}

/// The account is leaving, so an answer arriving now would be written after
/// the reset retired the file — and inherited by whoever pairs next.
#[test]
fn an_approval_is_refused_once_the_host_is_shutting_down() {
    let dir = TempDir::new("approve-late");
    dir.plugin("autoreply", &pong());
    let published = Published::default();
    let plugins = unapproved_host(&dir, Recorder::new(Outcome::Accepted), &published);
    published.settles("the plugin to be listed", |s| !s.is_empty());

    plugins.shutdown();
    plugins.approve("autoreply", true);
    assert!(
        !plugins.surfaces()[0].approved,
        "nothing is granted on the way out"
    );
}

/// A bit above `kinds::COUNT` means a plugin built against a newer ABI.
/// Adding a kind deliberately does not bump `VERSION`, so this is the only
/// thing that catches it — and masking the bit left the plugin loaded and
/// looking healthy while it never heard about what it asked for.
const SUBSCRIBES_TO_THE_FUTURE: &str = r#"(module
  (import "oxidezap" "oxi_subscribe" (func $subscribe (param i64)))
  (memory (export "memory") 1)
  (func (export "oxi_abi_version") (result i32) (i32.const $ABI_VERSION))
  (func (export "oxi_init") (result i32)
    ;; 1 << 40, far above any kind this host defines.
    (call $subscribe (i64.const 1099511627776))
    (i32.const 0))
  (func (export "oxi_on_event") (param i32) (param i32) (result i32) (i32.const 0))
)"#;

#[test]
fn a_subscription_to_a_kind_this_host_lacks_is_refused() {
    let dir = TempDir::new("future-kind");
    dir.plugin("ahead", &versioned(SUBSCRIBES_TO_THE_FUTURE));
    dir.plugin("autoreply", &pong());

    let published = Published::default();
    let plugins = unapproved_host(&dir, Recorder::new(Outcome::Accepted), &published);
    assert_eq!(
        plugins.ids(),
        vec!["autoreply"],
        "refused, and its neighbour still loads"
    );
}

/// `caps::ALL` promises the host refuses a bit outside it, and adding a
/// capability does not bump `VERSION` — so masking would leave a plugin
/// loaded with Settings showing a shorter sentence than the one it wrote.
const ASKS_FOR_THE_FUTURE: &str = r#"(module
  (import "oxidezap" "oxi_request_caps" (func $caps (param i64)))
  (memory (export "memory") 1)
  (func (export "oxi_abi_version") (result i32) (i32.const $ABI_VERSION))
  (func (export "oxi_init") (result i32)
    ;; A bit far above anything this ABI defines, beside one that exists.
    (call $caps (i64.const 1099511627784))
    (i32.const 0))
  (func (export "oxi_on_event") (param i32) (param i32) (result i32) (i32.const 0))
)"#;

#[test]
fn a_capability_this_host_lacks_is_refused() {
    let dir = TempDir::new("future-cap");
    dir.plugin("ahead", &versioned(ASKS_FOR_THE_FUTURE));
    dir.plugin("autoreply", &pong());

    let published = Published::default();
    let plugins = unapproved_host(&dir, Recorder::new(Outcome::Accepted), &published);
    assert_eq!(
        plugins.ids(),
        vec!["autoreply"],
        "refused rather than loaded asking for less than it wrote"
    );
}

/// Fuel prices one call. A plugin needs no permission to arm a timer, so an
/// unapproved one can wake itself forever, burn almost a full budget in each
/// callback and never trap — a core, permanently, for something subscribed to
/// no account event. The duty cycle is the bound on the sum.
const BURNS_ON_A_TIMER: &str = r#"(module
  (import "oxidezap" "oxi_request_caps" (func $caps (param i64)))
  (import "oxidezap" "oxi_timer_set"    (func $timer (param i64 i64) (result i32)))
  (import "oxidezap" "oxi_send_text"    (func $send (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "a@s.whatsapp.nettick")
  (func (export "oxi_abi_version") (result i32) (i32.const $ABI_VERSION))
  (func $spin
    (local $i i32)
    (block $done
      (loop $again
        (br_if $done (i32.ge_s (local.get $i) (i32.const 2000000)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $again))))
  (func (export "oxi_init") (result i32)
    ;; TIMERS needs nobody's yes; SEND is here only so each callback leaves a
    ;; mark the test can count.
    (call $caps (i64.const 33))
    (drop (call $timer (i64.const 100) (i64.const 1)))
    (i32.const 0))
  (func (export "oxi_on_event") (param $kind i32) (param $ev i32) (result i32)
    (drop (call $send (i32.const 0) (i32.const 16) (i32.const 16) (i32.const 4)))
    (call $spin)
    ;; And arm the next one, so it never runs out of work to do.
    (drop (call $timer (i64.const 100) (i64.const 1)))
    (i32.const 0))
)"#;

#[test]
fn a_plugin_that_wakes_itself_forever_is_held_to_its_share() {
    let dir = TempDir::new("duty");
    dir.plugin("greedy", &versioned(BURNS_ON_A_TIMER));
    let commands = Recorder::new(Outcome::Accepted);
    let published = Published::default();
    let plugins = host(&dir, Arc::clone(&commands), &published);
    published.settles("the plugin to be listed", |s| !s.is_empty());

    // Each callback marks itself before spinning, so this counts how many
    // actually ran. The bound is measured rather than guessed: over this
    // window an unthrottled plugin gets through sixteen callbacks and a
    // throttled one three, so six separates them with room on both sides —
    // twice what the throttle produced here and under half of what removing
    // it does, which is the margin a timing test on a loaded runner needs.
    std::thread::sleep(Duration::from_secs(6));
    let ran = commands.sent().len();
    assert!(
        ran <= 6,
        "a plugin waking itself forever ran {ran} times in six seconds, which is not a share"
    );
    assert!(
        plugins.surfaces()[0].is_running(),
        "held back rather than stopped: it is doing nothing wrong, only too much"
    );

    // And it lets go when asked, which is the property a sleeping worker
    // most easily breaks.
    let (done, waited) = std::sync::mpsc::channel();
    let plugins = Arc::new(plugins);
    let shutting_down = Arc::clone(&plugins);
    std::thread::spawn(move || {
        shutting_down.shutdown();
        let _ = done.send(());
    });
    waited
        .recv_timeout(Duration::from_secs(20))
        .expect("a throttled plugin is still one the daemon can join");
}
