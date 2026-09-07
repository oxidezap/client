# Browser call playout tests

This separate workspace imports the production output, statistics, and reporting
helpers. CI runs `stats` in `Check` and `browser` in `Test (web)`, and checks
this workspace's formatting separately. Run from the repository root.

The manifest's cargo-machete exception covers only `oxidezap-platform`.
`browser.rs` imports `../diagnostics.rs` through `#[path]`, outside the directory
cargo-machete scans, and that helper calls `oxidezap_platform::sleep`.

```bash
CARGO_TARGET_DIR=target/call-playout-tests cargo test \
  --manifest-path crates/audio/src/web/call_device/tests/Cargo.toml --locked --test stats

RUSTFLAGS='--cfg web_sys_unstable_apis' \
CARGO_TARGET_DIR=target/call-playout-browser \
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
  cargo test --manifest-path crates/audio/src/web/call_device/tests/Cargo.toml \
  --locked --target wasm32-unknown-unknown --test browser

# Inherits the root atomics/shared-memory flags and rebuilds std with them.
CARGO_TARGET_DIR=target/call-playout-shared \
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
  cargo +nightly test --manifest-path crates/audio/src/web/call_device/tests/Cargo.toml \
  --locked --target wasm32-unknown-unknown -Z build-std=std,panic_abort --test browser
```

Use wasm-bindgen-test-runner 0.2.127 and matching Chrome/ChromeDriver versions.
Set `CHROMEDRIVER` to the driver executable and
`WASM_BINDGEN_TEST_WEBDRIVER_JSON` to a local capabilities file. The runner does
not serve isolation headers. Enable shared memory for these tests with Chrome's
`--enable-features=SharedArrayBuffer` flag. A capabilities file can use this form,
with `binary` pointing to your Chrome executable.

The shared-memory rejection test needs that flag even in the ordinary stable
build. CI supplies it through a temporary capabilities file; the nightly shared
build remains a separate local check.

```json
{"goog:chromeOptions":{"binary":"/path/to/chrome","args":["--enable-features=SharedArrayBuffer"]}}
```

## Evidence

The pre-fix reproductions passed before the production edit. The legacy warning
predicate emits 500 warning decisions for 1,000 alternating full/empty callbacks.
At a synthetic 20 ms cadence, the replacement produces three interval summaries
and one final total, with 500 starved blocks, 500 runs, and 512,000 missing samples.
The browser logger test verifies zero logs during counter/error collection and
bounded reports afterward. The timer test drives the real platform wait and
checks cancellation on both completion and future drop.

The browser allocation probe counts Float32Array constructors that allocate sample
storage, not constructors or subarrays that only create views. The legacy path
allocates 1,000 sample buffers for 1,000 blocks. The production helper allocates
one at setup, then none across 1,000 full, partial, and empty blocks, including
WASM memory growth halfway through. Every sample is checked for correct copy or
zero fill. The shared build also asserts that the test module's own memory is
shared. A separate test verifies that WebAudio refuses a shared WASM view but
accepts an owned copy.

At 48 kHz and 1,024 samples per block, eliminating one 4,096-byte allocation per
block avoids 187.5 KiB/s of sample-buffer allocation demand. This is arithmetic,
not a measured reduction in CPU time or total JS allocations. The WASM-to-JS copy,
the WebAudio copy, generated temporary views, and capture allocations remain.

## Diagnostic bounds

Diagnostics are enabled at graph creation when warning logging is enabled. When
disabled, the playout callback reads no diagnostic clock and updates no counters.
Enabled diagnostics exclude silence before the first feed. A callback is counted
late when its arrival gap exceeds the nominal block period by at least another
whole block. This measures callback scheduling gaps, not network delay.

Reports wait five seconds after each report, without catch-up bursts. A delayed
page can therefore delay a report. Teardown detaches handlers, flushes one final
total, and cancels the reporting wait. Speaker write failures are retained once
and formatted outside the callback, at the next report or teardown.

The tests do not measure live-call audio quality, hardware scheduling, browser
throttling, or end-to-end latency. They do not run the full microphone graph.
Startup prime duration, ring ceiling, capture behavior, codecs, and native audio
remain unchanged.
