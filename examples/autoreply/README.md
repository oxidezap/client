# autoreply

An example oxidezap plugin. It answers messages containing a keyword, and puts
its own settings on the Settings screen.

```bash
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown

mkdir -p ~/.local/share/oxidezap/plugins
cp target/wasm32-unknown-unknown/release/autoreply.wasm ~/.local/share/oxidezap/plugins/
```

Restart `oxidezapd`. The file's name is the plugin's id, so the copy above
loads as `autoreply`.

Set `OXIDEZAP_PLUGIN_DIR` to load from somewhere else, which is what you want
while writing one.

## Testing it

Its handlers run under `cargo test`, against the SDK's test host rather than
the daemon:

```bash
cargo test
```

That host answers the same sixteen imports from a table the test owns, so a
handler can be given a message and asked what it did. It is not the daemon —
nothing there enforces fuel, capabilities or approval, and a command is
recorded rather than sent — so a handler that passes here can still be refused
in the sandbox. What it checks is the half you wrote.

## What it shows

* **Declaring itself, once.** `setup` names the plugin, subscribes to messages
  only, and asks for exactly three capabilities. That list is what a user is
  shown before they enable it, so asking for less is the whole point of
  asking — and each of the three may be said only once, which the *type*
  enforces: `name` is not a method on what `name` returns.
* **Reading the fields its kind actually has.** `ev.which()` narrows the event
  to a `Message` or an `Action`, so asking a message for a widget id is a
  method that is not there rather than an empty string nothing questions.
* **Reading an event without allocating.** `Text<N>` is a fixed buffer on the
  stack; a field nobody reads costs nothing at all. The `N` comes from the
  field rather than from the call site, and `whole()` is how a read says
  "all of it or nothing" — a JID that did not fit is not a shorter JID, it is
  somebody else.
* **Publishing an interface.** The tree is built with `ui::publish` into a
  buffer this crate owns — no allocator anywhere — and republished whenever
  something in it changes. A section takes a closure, so there is no `end` to
  forget and the widgets inside it have no slot to pass.
* **Logging a value.** `log!` formats into a fixed buffer — no heap — which
  costs about 2.6 KiB of `core::fmt` and is why this plugin is 8 KiB rather
  than 5.4. A constant line through `log` is free.
* **Answering its own widgets.** A `UI_ACTION` arrives as an ordinary event
  and the toggle's new state is written straight to storage.
