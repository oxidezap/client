# WebCodecs readback tests

These browser tests import the production `webcodecs.rs`, `geometry.rs`, and
`sps.rs`. They exercise the actual output callback, active/pending worker,
generation checks, buffer reuse, conversion, and publication.

The fixture replaces `VideoDecoder` only while constructing a decoder. It sends
synthetic, real `VideoFrame` objects through the registered output callback.
Each `copyTo` executes in the browser immediately, but a promise gate withholds
its completion until the test releases it. Tests await the real copy and drain
the promise continuations before asserting results. No camera or H.264 encoder
is needed.

The only Rust stand-in is `RenderImage`. It retains the actual image pixels and
counts constructions, without linking GPUI's window system or shared-memory
executor. A destination-length getter also detects `to_vec` on stale copies
when no later copy is reusing that destination.

## Run

Run from the repository root with stable Rust, the wasm target, Chrome, and a
matching chromedriver. Use `wasm-bindgen-test-runner` **0.2.127**, matching this
crate's lockfile and the existing `Test (web)` CI job. See
[`docs/building.md`](../../../../../docs/building.md) for installation details.

```bash
CHROMEDRIVER="${CHROMEWEBDRIVER:-}/chromedriver"
test -x "$CHROMEDRIVER" || CHROMEDRIVER=$(command -v chromedriver)
export CHROMEDRIVER
RUSTFLAGS='--cfg web_sys_unstable_apis' \
CARGO_TARGET_DIR=target/webcodecs-tests \
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
  cargo test --manifest-path crates/gui/tests/webcodecs/Cargo.toml \
    --locked --target wasm32-unknown-unknown --test readback
```

The `Test (web)` CI job runs this command after the daemon and session browser
tests, using the same runner. The separate workspace avoids their dependencies
and the GUI's shared-memory build. The explicit `RUSTFLAGS` replaces the root
wasm flags, as in the existing browser
CI job. Keep the bindgen versions here in step with that job when updating them.

If Chrome is not installed in its default location, set
`WASM_BINDGEN_TEST_WEBDRIVER_JSON` to an absolute path to a local file containing
`{"goog:chromeOptions":{"binary":"/absolute/path/to/chrome"}}`.
`CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER` can likewise name an absolute path
to the matching runner. Do not commit machine-specific paths.

## Coverage

- A active, B pending, C replacing B, then A completes and C publishes.
- Replaced frames close without copying, while every compressed input is fed.
- Reset clears pending output without admitting a second active copy.
- Drop and out-of-order completion skip Rust materialization and publication.
- Copy rejection closes active/pending frames and reset restores progress.
- A rejection from the previous generation does not fail the reset decoder.
- Equal-sized destinations are reused; a resolution change replaces the buffer.
- A pending frame retains the rotation recorded when its input was fed.
- Configuration failure closes the decoder before callback ownership ends.

These tests do not measure camera performance, decode a compressed reference
chain, exercise a hardware decoder, or draw through GPUI. They run without the
application's shared-memory configuration. They do not validate native BGRA
copy support. The RGBA readback and BGRA pixel conversion are real.
