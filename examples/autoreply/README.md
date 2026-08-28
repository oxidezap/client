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

## What it shows

* **Declaring itself.** `setup` names the plugin, subscribes to messages only,
  and asks for exactly three capabilities. That list is what a user is shown
  before they enable it, so asking for less is the whole point of asking.
* **Reading an event without allocating.** `Text<N>` is a fixed buffer on the
  stack; a field nobody reads costs nothing at all.
* **Publishing an interface.** The tree is built with `abi::ui::Writer` into a
  buffer this crate owns — no allocator anywhere — and republished whenever
  something in it changes.
* **Answering its own widgets.** A `UI_ACTION` arrives as an ordinary event
  and the toggle's new state is written straight to storage.
