# WebCodecs tests

These browser tests import the production `webcodecs.rs`, `geometry.rs`,
`sps.rs`, and `h264.rs`. The `readback` target exercises the output callback,
active/pending worker, generation checks, buffer reuse, conversion, and
publication. The `recovery` target passes browser-generated H.264 through the
production admission helper and a real `VideoDecoder`.

The readback fixture replaces `VideoDecoder` only while constructing a decoder. It sends
synthetic, real `VideoFrame` objects through the registered output callback.
Each `copyTo` executes in the browser immediately, but a promise gate withholds
its completion until the test releases it. Tests await actual copy entry before
resetting, dropping, or checking the active/pending order. Releasing a copy waits
for Rust to close its frame or enter a fallback copy on that same frame. Neither
wait polls nor releases a gate; a five-second deadline fails missing milestones.
Exact copy-count, pixel, and publication assertions remain separate from the waits.
No camera or H.264 encoder is needed.

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
    --locked --target wasm32-unknown-unknown --test readback --test recovery --test transform
```

Use explicit test targets in the `Test (web)` CI job with the same runner
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

To exercise the atomics scheduler too, use nightly with `rust-src` installed and
the root's shared-memory flags, without the ordinary run's `RUSTFLAGS` override:

```bash
env -u RUSTFLAGS \
  CARGO_TARGET_DIR=target/webcodecs-shared \
  CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
  cargo +nightly test --manifest-path crates/gui/tests/webcodecs/Cargo.toml \
    --locked --target wasm32-unknown-unknown -Z build-std=std,panic_abort \
    --test readback --test recovery --test transform
```

Runner 0.2.127 serves isolation headers for the shared module. With Chrome and
ChromeDriver 153.0.8010.12, the page exposed `SharedArrayBuffer` without a browser
feature override. This still does not run GPUI's worker pool or upload images.

The old readback wait used one zero-delay timer. It returned before the first
copy in the shared build, whose pending Rust futures resume through
`Atomics.waitAsync`. The BGRA probe adds promise continuations before that copy.
The shared suite reproduced 14 failures and two passes, while ordinary wasm
passed all 16. Copy-entry and consumption milestones passed all 16 in both builds;
adding another timer would not establish the required ordering.

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
whether Chrome selects hardware or software codecs. These targets do not measure
camera performance or draw through GPUI. The ordinary command omits the
application's shared-memory configuration; the nightly command includes it.
RGBA and BGRA copies are real; older-browser behavior is injected around the
native copying method.

## Readback benchmark

Select only `--test readback`, add `--release --features benchmarks`, and append
`production_readback_benchmark -- --nocapture`. Set
`WASM_BINDGEN_TEST_TIMEOUT=120` for the longer run.

The current benchmark also measures generated H.264 output and reports separate
synchronous-call and promise-completion times. See the decoded-frame follow-up
below for its parameters. The following measurements used the earlier fixture.

Two production decoders alternated readbacks from clones of the same synthetic
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

## Decoded-frame follow-up

The benchmark now generates moving canvas rectangles at 1280x720 with a 20 fps
deadline and 50,000 microsecond timestamps. A real `VideoEncoder` produces
Annex-B baseline H.264, `avc1.42001f`, at a requested 2.5 Mbps with a keyframe
every 20 pictures. Each encoded picture goes through a real `VideoDecoder`.
Two clones of that decoded frame then enter the existing production readback
callback, alternating RGBA-first and BGRA-first. There is no camera, captured
media, decoder-output replacement, or promise gate in this source path.
The two production callback owners still use the fixture's inert decoder during
construction, so the readback comparison does not decode the picture twice.

There are 20 warmup pictures and 120 measured pictures per path, followed by
raw RGBA and I420 controls at the same resolution. Those controls are unpaced
and contain constant pixels, so compare formats within each row rather than
absolute times between source types. First and last pairs verify every copied
pixel after accounting for channel order, outside the timed interval. Every
copy checks its byte count. Each measured path copies 442,368,000 bytes across
120 frames, 3,686,400 bytes per frame. Warmup copies are additional.

The report distinguishes these intervals, in milliseconds.

- `callMs` measures entry to return of the native `copyTo` call.
- `copyMs` measures entry to the first promise continuation.
- `postCopyMs` measures that continuation to publication completion. It includes
  JS-to-wasm materialization, optional Rust swizzle, image construction and
  continuation overhead, not an isolated timer around `to_vec`.
- `outputMs` measures production output-callback entry to publication completion.

Source generation and encoding are outside the per-frame readback timer. The
encoder flushes each submitted picture. The steady source decoder uses
`optimizeForLatency: true` to deliver one picture without waiting for later
input. Without it this serialized fixture waited indefinitely for its first
output; that was a fixture deadlock, not measured readback starvation. This
setting differs from production decoder configuration and does not change the
imported production readback helper. Output still goes through the real
JS-owned destination reuse and Rust conversion, with only `RenderImage` replaced
as described above. Shared-memory execution and GPUI upload remain outside scope.

### Browser setup

Measured on 2026-09-07 with Chrome for Testing and ChromeDriver
151.0.7922.34, optimized Rust wasm, and bindgen runner 0.2.127. The system runner
had become 0.2.128, so a separate 0.2.127 installation was used. Do not update the
test lockfile to work around a runner mismatch.

Use the command above with this WebDriver configuration, substituting the local
Chrome 151 binary path. `--enable-gpu` is necessary here because the existing
headless defaults selected SwiftShader. No VAAPI, ANGLE, driver-check override,
or software-decoding switch was added to the measured GPU-enabled runs.

```json
{"goog:chromeOptions":{"binary":"/absolute/path/to/chrome151","args":["enable-gpu","remote-debugging-port=9335"]}}
```

```bash
WASM_BINDGEN_TEST_WEBDRIVER_JSON=/absolute/path/to/webdriver.json \
WASM_BINDGEN_TEST_TIMEOUT=120 \
CHROMEDRIVER=/absolute/path/to/chromedriver151 \
RUSTFLAGS='--cfg web_sys_unstable_apis' \
CARGO_TARGET_DIR=target/webcodecs-tests \
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=/absolute/path/to/wasm-bindgen-test-runner-0.2.127 \
  cargo test --manifest-path crates/gui/tests/webcodecs/Cargo.toml \
    --locked --target wasm32-unknown-unknown --test readback \
    --release --features benchmarks production_readback_benchmark -- --nocapture
```

The runner additionally supplies headless, no-sandbox and disable-dev-shm-usage;
ChromeDriver supplies its normal automation flags. Port 9335 is only for optional
CDP observation and can be omitted for timing-only runs. While the test runs,
use browser-session `SystemInfo.getInfo` and page-session `Media.enable`, then
record `Media.playerPropertiesChanged`. Support queries alone do not identify
the codec implementation actually chosen.

In the observed benchmark browser, CDP reported Intel device 32103 and NVIDIA
device 11608, Mesa 26.2.2, with GPU compositing, rasterization, 2D canvas and
WebGL enabled. The generic `video_decode` status was `enabled`, but the
`videoDecoding` profile list was empty. Both encoder and decoder support queries
returned true for `no-preference` and `prefer-software`, false for
`prefer-hardware`. The hardware row explicitly reports zero frames and bytes.
CDP identified `OpenH264VideoEncoder` and `FFmpegVideoDecoder`, with
`kIsPlatformVideoEncoder` and `kIsPlatformVideoDecoder` both false. Decoded
frames reported I420, BT709 limited range and a 1280x720 visible rectangle.

These are real decoded frames with GPU rendering enabled, but they are **not
hardware-decoder-backed frames**. An earlier run without `enable-gpu` also
reported hardware support false and used SwiftShader. A separate capability
inspection with Linux accelerated-decode feature flags still exposed no decode
profiles; it is not included in the timing results.

### Results

Four GPU-enabled runs gave these mean publication times. Entries are RGBA to
BGRA in milliseconds. Run C also had the read-only CDP observer attached.
Run D verified the added full-pixel comparisons and per-copy byte assertions.

| Source | Run A | Run B | Run C | Run D |
| --- | --- | --- | --- | --- |
| Decoded, no preference | 3.612 to 3.328 | 3.565 to 3.351 | 3.890 to 3.595 | 3.439 to 3.175 |
| Decoded, prefer software | 3.340 to 3.030 | 3.644 to 3.511 | 3.452 to 3.279 | 3.118 to 2.880 |
| Raw RGBA | 3.561 to 3.287 | 3.950 to 3.644 | 2.567 to 2.390 | 6.209 to 6.065 |
| Raw I420 | 9.899 to 9.250 | 9.358 to 8.797 | 7.453 to 6.914 | 4.641 to 4.456 |

Run C separates the decoded-frame work as follows. All values are means.

| Source | Format | Synchronous call | Promise completion | Post-copy | Publication |
| --- | --- | --- | --- | --- | --- |
| No preference | RGBA | 2.587 | 2.606 | 1.241 | 3.890 |
| No preference | BGRA | 2.691 | 2.711 | 0.842 | 3.595 |
| Prefer software | RGBA | 2.265 | 2.281 | 1.129 | 3.452 |
| Prefer software | BGRA | 2.451 | 2.466 | 0.776 | 3.279 |

Most of the copy time was synchronous. BGRA increased decoded `copyTo` mean
time by 2.5-9.0% across these runs, while publication mean time decreased by
3.6-9.3%. The post-copy savings exceeded the copy penalty. Machine load varied;
the sequential raw-control rows are not evidence that I420 is intrinsically
slower than decoder-produced I420. Run C encoded and decoded 140 pictures per
source, totaling 727,530 and 723,998 compressed bytes respectively. Each row
measured 120 complete copies per format after warmup, with no resolution change.

### Pending-loop inspection

The benchmark also submits the first 66 compressed pictures as a burst through
a fresh real decoder into each production callback. It uses the production
default `optimizeForLatency: false` for this check. The actual active/pending
worker closes and publishes the frames. Timer and animation callbacks count
progress during the burst. These sequential bursts diagnose scheduling; their
elapsed times are not an interleaved format comparison.

In run C each burst decoded, copied, closed and published all 66 pictures,
243,302,400 copied bytes per burst.

| Decoder preference | Format | Elapsed ms | Timer ticks | Animation callbacks | Maximum timer gap ms |
| --- | --- | --- | --- | --- | --- |
| No preference | RGBA | 454.010 | 72 | 28 | 21.660 |
| No preference | BGRA | 263.760 | 70 | 16 | 7.715 |
| Prefer software | RGBA | 300.715 | 68 | 19 | 10.185 |
| Prefer software | BGRA | 199.415 | 52 | 12 | 10.910 |

`webcodecs.rs` takes the pending frame immediately after `read_frame(...).await`
without a task yield. A resolved copy promise therefore does not guarantee a
rendering opportunity before the pending copy. However, the pending slot holds
only one frame, and real decode callbacks need task delivery to replenish it.
These bursts did not reproduce an indefinitely replenished microtask chain.
They also did not exercise pending replacement under slow GPU readback, since
all 66 frames were copied. They establish timer/render-callback progress in the
tested software path, not a bound for every decoder or the application's other
microtask producers. No bounded-yield alternative was tested or added.

### Recommendation

Do not infer a BGRA-specific GPU/vendor regression from these runs, and do not
treat them as evidence that the hardware path is safe. That comparison remains
blocked by unavailable hardware decoding in this Chrome 151 setup. The supplied
trace percentages were not independently reprocessed, and percentages of
different captures do not establish per-frame cost without counts and duration.
The measured software path does not justify a BGRA revert. The burst results do
not justify adding a yield to fix demonstrated starvation. A hardware-backed
paired readback result or a production-equivalent starvation reproduction is
still needed before choosing either production change.
