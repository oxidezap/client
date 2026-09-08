# Media recovery and readback follow-up

The PR 139 preview recording showed shorter main-thread tasks, but still contained
11 remote decoder failures and repeated audio underruns. It began during a call
and ended before hangup, so it did not verify camera shutdown. Raw logs and
profiling data remain private.

## H.264 admission

The upstream transport's keyframe flag can describe SPS/PPS-only access units.
Those units configure a decoder but cannot restart its picture reference chain.
Native and web call decoders now inspect the bitstream rather than treating that
flag as proof of recovery.

The shared helper retains bounded parameter sets by ID and resolves each IDR
slice's PPS-to-SPS references. A WebCodecs Annex-B key chunk contains its required
sets, including when they arrived separately. Valid parameter-only updates do
not reset an active decoder. Their declarations precede the next picture, and
sets announced for later delta pictures are preserved. Ordinary deltas and
complete in-band IDRs can borrow their input rather than rebuilding it.

An invalid admission header or unresolved IDR reference returns to recovery.
The helper checks leading headers, parameter references, and the pixel budget;
it is not a full H.264 validator. Decoders still handle malformed picture data.
Tests compare direct decoding with prepared streams using OpenH264 and real
browser codecs, including repeated parameters and a later delta selecting a
different PPS. No captured media is used.

Recovery diagnostics report bounded request attempts, queue admission, IDR
admission, and first output. Queue admission is not an acknowledgment from the
peer, and first decoder output is not proof of presentation on screen.

## Browser readback

Unrotated pictures use BGRA output when a cached one-pixel probe verifies both
support and channel order. Browsers that reject or silently ignore the format
option use RGBA. A real-frame BGRA copy refusal retries RGBA while the frame is
still open. Rotation keeps the existing fused RGBA-to-BGRA rotation path.

The asynchronous destination remains JS-owned. The active/pending bounds,
generation checks, dimensions, frame rate, and encoded quality are unchanged.

Four interleaved synthetic Chrome runs measured 6.4-10.5% lower mean time from
output callback to publication with BGRA. The tests used optimized WASM, identical
640x360 and 1280x720 RGBA/I420 inputs, 50 warmup pictures, and 1,000 measured
pictures per path. The removed Rust swizzle accounts for the benefit; `copyTo`
itself did not consistently get faster. This is not a live-call FPS or CPU result.
The [browser test README](../crates/gui/tests/webcodecs/README.md) records the
benchmark commands, results, fallbacks, and limitations.

## Audio callbacks

The browser playout callback now reuses a JS-owned sample buffer and Rust scratch
storage. It does not log or format write errors. A cancellable platform wait
reports starvation and scheduling counters at five-second intervals, with one
final total on teardown. The first speaker-write error is retained for reporting
even when statistics are disabled.

An alternating 1,000-block test previously made 500 warning decisions. Reporting
the same events at a synthetic 20 ms cadence produces three interval summaries
and one final total. Sample-buffer backing allocations fall from 1,000 to one
at setup, with no further sample-buffer allocations during the test. Copies and
temporary binding views still exist. Startup priming, buffering ceilings, codec
negotiation, and capture behavior are unchanged.

Real-browser tests verify sample values, zero filling, reuse after WASM memory
growth, rejection of shared views by WebAudio, and cancellation of reporting.
They run locally in ordinary and shared-memory builds. CI runs the ordinary
browser configuration and native statistics tests. See the
[playout test README](../crates/audio/src/web/call_device/tests/README.md).

Successful active-frame and SID messages from MLow now use `trace` rather than
`debug`, through merged [whatsapp-rust#1466](https://github.com/oxidezap/whatsapp-rust/pull/1466).
Warnings, rejected operating points, and failure diagnostics remain unchanged.
Its tests compare decoded PCM and frame reports across logging levels.

## Remaining work

These fixes do not establish complete WhatsApp receive-policy parity or eliminate
network loss. The upstream receive queue requests recovery after overflow but
does not propagate that loss to the next delivered access unit. Its depacketizer
also needs separate whole-access-unit loss coverage. Correcting those contracts
requires an upstream change; this patch does not infer loss from timestamps or
add another PLI trigger.

Audio still uses ScriptProcessorNode, not AudioWorklet. Video still crosses CPU
memory before texture upload. Those architectural changes need separate runtime
and performance evidence. The local-camera-off/remote-reception limitation
documented in [video lifecycle](video-lifecycle.md) remains. No native daemon CPU
profile was recorded for this iteration.

## Preview retest

The September 7 15:57 trace and accompanying log both name bundle
`565296d0f6a1efd4` from PR 140. The previous 13:33 capture names
`c10b5cdcb2f09341` from PR 139. Matching trace/log assets do not establish the
deployed Git commit or equal video workloads.

| Recorded metric | PR 139 capture | PR 140 capture |
| --- | ---: | ---: |
| Active interval | 27.879 s | 24.616 s |
| Main task wall occupancy | 66.14% | 99.68% |
| Main task CPU, fraction of one core | 36.21% | 48.76% |
| Sampled elapsed attribution to `copyTo` | 39.42% | 64.76% |
| RAF callback duration p95 | 7.272 ms | 12.596 ms |

The retest has worse recorded responsiveness. Its five remote decoder failures
are each followed by a queued request, an admitted IDR, and first decoder output.
That proves recovery output, not uninterrupted playback or presentation. Audio
reports 999 starved blocks among 2,094 callbacks across nine intervals, with
584,258 missing samples and no logged speaker-write error. The capture ends
before camera teardown.

Neither trace records copy invocation counts, formats, bytes, or per-copy source
dimensions. The increased `copyTo` share therefore cannot distinguish more work
from slower individual copies. Profiling itself consumes substantial CPU.
Follow-up benchmarks using real software-decoded frames still favor BGRA total
publication time, but no hardware decoder was available locally. Neither a
BGRA revert nor an added event-loop yield is justified by those measurements.

Debug-enabled call decoders now report cumulative readback totals at most once
per five seconds of output activity, plus a frozen final snapshot on teardown.
The call, direction and first input stamp identify each decoder lifetime.
Counters include decoded outputs, copy attempts and completions, materialized
images and bytes, visible dimensions, discarded output, formats, and fallbacks.
Attempts include rejected copies and retries, but not the capability probe.
They are workload counters, not per-copy duration measurements. Compare deltas
within one decoder lifetime rather than summing cumulative reports.

## Capture ownership

`MediaStream.clone` is the browser method and duplicates tracks; it is not a
Rust handle copy. The camera preview and the microphone graph both cloned the
acquired stream and stopped only one track set on teardown, leaving the
original capturing with the tab indicator on. Both paths now borrow the guarded
stream and move its tracks into the owner that stops them.

Two adjacent microphone paths needed guards of their own. A permission grant
landing after the opener was dropped or timed out still opens the device, so a
late grant is stopped unless the opener synchronously claimed the stream. A
setup dropped while the prompt is up also closes the audio context, which the
call graph never came to own. A playout-node creation failure detaches the
already-armed capture handler before its closure goes away.

`crates/audio/tests/call_audio_lifecycle.rs` drives the production opener
through granted, failing and cancelled setups in a real browser and requires
every track ended, every handler detached and every context closed. The camera
harness in `crates/session/src/video/camera_lifecycle_tests.rs` counts
`MediaStream` and track clones and likewise requires zero surviving tracks,
timers and callbacks. Neither exercises a physical camera LED.

## Received orientation

Switching the Android camera from front to rear turned the web picture upside
down. The library stamped every received frame with the last signaling
`device_orientation`, ignoring the per-frame RTP rotation. The executed
receive oracle maps all 256 frame-info bytes to clockwise display turns of 0,
270, 180 and 90 degrees for low bits 0, 1, 2 and 3. The upstream correction
merged in
[whatsapp-rust#1469](https://github.com/oxidezap/whatsapp-rust/pull/1469),
which is included in the merged revision consumed below. No 180-degree GUI
compensation was added. A live Android front/rear retest is still required.

The parser correction merged in
[whatsapp-rust#1470](https://github.com/oxidezap/whatsapp-rust/pull/1470)
at `2681a687c35f8fc290c1ff62bb30a857a12df9e0`. The client currently consumes
`test/voip-call-fixture` at `a45320848ce4808bdcc2b4a7436f2a7f1e0c66a0`, based on
that commit, for source-bearing call events and the native fixtures in
[whatsapp-rust#1472](https://github.com/oxidezap/whatsapp-rust/pull/1472).
The client-side ordering tests are described in [video lifecycle](video-lifecycle.md).
All WhatsApp dependency declarations use the same branch and the root lockfile
records the revision. The fixture adds no production call-policy correction.

Authenticated receive packets now expose frame-info independently of optional
timing and bandwidth extensions. Upstream compared 3,840 synthetic packets and
285 boundary cases against the captured WASM parser, covering reordered,
frame-info-only and extended layouts, duplicate precedence, padding and
truncation. The outgoing extension layout and signaling fallback are unchanged.
The client adds no production rotation policy. Upstream's separate passthrough
test checks Annex-B bytes and renderer arguments, not decoded pixels, and its
observed packet-metadata OR aggregation is not part of this fix. The live Android
extension layout remains uncaptured, so the parser discrepancy does not prove
the cause of the reported rear-camera image.
