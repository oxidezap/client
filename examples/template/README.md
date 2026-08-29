# template

A starting point for an oxidezap plugin. Copy the directory, change the name
in `Cargo.toml` — that name is the plugin's id — and delete what you do not
need.

```bash
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown

mkdir -p ~/.local/share/oxidezap/plugins
cp target/wasm32-unknown-unknown/release/template.wasm ~/.local/share/oxidezap/plugins/
```

Restart `oxidezapd`. Set `OXIDEZAP_PLUGIN_DIR` to load from somewhere else,
which is what you want while writing one.

## What it does

Counts the messages it is given, draws the count on the Settings screen, and
gives you a button to reset it. It asks for nothing that touches the account,
so it runs the moment it is dropped in the folder — adding `Caps::SEND` is
what puts a question in front of the user, and nothing that acts on the
account works until they answer it.

## What it is showing you

* **`plugin!`** generates the three exports the host looks for *and* the
  `#[panic_handler]` a `no_std` wasm module cannot link without. Pass
  `panic = own` to write your own.
* **`setup` declares once.** A name, a subscription, a list of capabilities —
  each may be said only once, and the type says so: `name` is not a method on
  what `name` returns.
* **`which()` narrows an event** to the fields its kind carries, so reading
  the wrong one is a method that is not there rather than an empty string.
* **`log!` formats without a heap.** It costs about 2.6 KiB, because
  formatting pulls `core::fmt` in; a constant line through `log` costs
  nothing.
* **`cargo test` runs the handlers** against the SDK's test host. No daemon,
  no wasm toolchain, no copying files.

## Where the rules are

`docs/plugin-abi.md` is the wire contract — what you need if you are writing a
plugin in something other than Rust. `examples/autoreply` is the same shape
with something actually in it.
