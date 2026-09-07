# WebCodecs tests

These browser tests import the production `webcodecs.rs`, `geometry.rs`,
`sps.rs`, and `h264.rs`. The `readback` target exercises the output callback,
active/pending worker, generation checks, buffer reuse, conversion, and
publication. The `recovery` target passes browser-generated H.264 through the
production admission helper and a real `VideoDecoder`.

The readback fixture replaces `VideoDecoder` only while constructing a decoder. It sends
synthetic, real `VideoFrame` objects through the registered output callback.
Each `copyTo` executes in the browser immediately, but a promise gate withholds
its completion until the test releases it. Tests await the real copy and drain
the promise continuations before asserting results. No camera or H.264 encoder
is needed.

The recovery fixture uses real `VideoEncoder` and `VideoDecoder` instances,
without replacing either global. It encodes two synthetic 32x32 canvas pictures
as Annex-B AVC. Tests admit an IDR and its delta, reset and reconfigure the
decoder, then decode that reference chain again. A second test caches SPS/PPS
in separate units and reconstructs a key chunk with leading AUD/SEI. The tests
check output timestamps and dimensions and close every frame and codec. They
require browser AVC encoding and decoding support; unsupported codecs fail
rather than silently skipping the proof. No captured media or camera is used.

Recovery tests also repeat parameter-only units between IDR and P pictures and
announce PPS 1 for a later P picture, both with the IDR and separately afterward.
The shared `h264_fixture.rs` edits only PPS IDs in generated baseline CAVLC
headers. Direct decoding verifies those edited streams before testing the
production helper. Native call tests check that these sequences produce two
pictures without requesting recovery.

The helper distinguishes valid parameter-only updates from invalid input. It
retains updates for the next picture instead of resetting an active decoder.
IDRs retain all validated cached declarations, including those a later delta
may reference after reset. Complete in-band units with the active SPS first
and ordinary deltas are borrowed without rewriting. Pending updates are
inserted before the next picture. Cached IDs resolve replacements, so injection
never combines an old definition with a new definition of the same ID.

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
    --locked --target wasm32-unknown-unknown --test readback --test recovery
```

Use both explicit test targets in the `Test (web)` CI job with the same runner
as the daemon and session tests. Do not substitute `--all-targets`, which also
builds the library's imported native unit tests requiring OpenH264.
The separate workspace avoids the daemon and session dependencies
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
- Real BGRA copies preserve colors; all four orientations preserve every pixel
  of a 3x5 frame through both the supported and fallback paths.
- A browser rejecting BGRA or silently ignoring `format` selects cached RGBA.
- A real-frame BGRA rejection retries RGBA in the same JS destination, without
  publishing early or marking the decoder failed. Reset cancels that retry.

The recovery target decodes compressed reference chains but does not assert
whether Chrome selects hardware or software codecs. Neither target measures
camera performance or draws through GPUI. Both run without the application's
shared-memory configuration. RGBA and BGRA copies are real;
older-browser behavior is injected around the native copying method.

## Readback benchmark

Select only `--test readback`, add `--release --features benchmarks`, and append
`production_readback_benchmark -- --nocapture`. Set
`WASM_BINDGEN_TEST_TIMEOUT=120` for the longer run.

Two production decoders alternate readbacks from clones of the same synthetic
frame. The baseline's capability probe is rejected once to select the shipped
RGBA-plus-swizzle path. The candidate uses native BGRA. Both use the production
output callback, JS destination reuse, JS-to-wasm copy and image construction.
The fixture does not gate benchmark promises. Input allocation is outside the
timer. Each pair alternates execution order, discards 50 warmup frames and
measures 1,000 frames per path. The report includes mean, median and p95 elapsed
milliseconds for real `copyTo` and output-callback-to-sink publication. First
output is reported separately, including the capability probe. No production
timers or per-frame logging were added.

Measured with optimized Rust wasm, HeadlessChrome 151 on Linux x86_64 and an
Intel Core Ultra 9 275HX. Four interleaved runs gave these mean publication times.
Each cell is RGBA baseline to BGRA candidate, in milliseconds.

| Source | Size | Run 1 | Run 2 | Run 3 | Run 4 |
| --- | --- | --- | --- | --- | --- |
| RGBA | 640x360 | 0.986 to 0.922 | 1.102 to 1.020 | 2.061 to 1.872 | 1.004 to 0.920 |
| RGBA | 1280x720 | 5.754 to 5.383 | 5.487 to 5.035 | 4.605 to 4.188 | 6.201 to 5.770 |
| I420 | 640x360 | 1.146 to 1.056 | 1.660 to 1.485 | 0.620 to 0.577 | 2.352 to 2.151 |
| I420 | 1280x720 | 3.376 to 3.090 | 3.023 to 2.765 | 6.006 to 5.529 | 4.560 to 4.240 |

The candidate reduced mean publication time by 6.4-10.5%. It did not make
`copyTo` faster. In run 2, 720p I420 `copyTo` increased from 1.852 to 2.037 ms,
while publication decreased from 3.023 to 2.765 ms because the Rust swizzle pass
was removed. Rotated output still uses the existing fused RGBA rotation and
swizzle, so no additional rotation pass or changed orientation is introduced.

Run 3 also recorded first output per decoder. The BGRA path, including its
probe, took 1.550/4.945 ms for 360p/720p RGBA and 0.885/3.280 ms for I420.
These are single observations in an already-running browser, not startup
latency estimates. The benchmark baseline includes an artificial probe
rejection, so its first output is not the shipped version's cold latency.
Run 4 confirmed the actual requested copy formats as RGBA and BGRA. Its first
720p I420 output took 6.395 ms for baseline and 7.490 ms for candidate; the
first baseline RGBA output took 2.526 seconds. These first-output variations
are why the steady-state comparison excludes warmup and makes no startup claim.

Absolute times varied with concurrent machine load. Earlier sequential runs
were inconclusive and are not used as evidence of improvement. These are
controlled synthetic readback comparisons, not a replay of the live trace
reporting 39.4% elapsed time in `copyTo`. They establish neither a live-call FPS
increase nor a reduction in that trace percentage. Hardware-backed decoded
frames, shared-memory application execution and GPUI upload remain unmeasured.

The [WebCodecs copy options](https://www.w3.org/TR/webcodecs/#videoframecopytooptions)
allow BGRA with tightly packed default layout. Installed `web-sys` 0.3.104
exposes `VideoPixelFormat::Bgra`, `set_format`, allocation sizing and the
promise-returning JS-buffer copy binding. Because an older browser can ignore
an unknown dictionary member, production verifies a 1x1 color sample rather
than treating promise success as support. The probe and any downgrade are
cached per decoder and survive reset. A backing-store-specific rejection still
retries the same open frame as RGBA before setting failure.
